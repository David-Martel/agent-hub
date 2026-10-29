"""Export the agent-bus history from this hub and push it to the agent-hub SFTP drop(s).

Owner directive 2026-10-29: agent-hub items live on dtm-* hosts, not on the hub.
This job exports the hub's full history, pushes it to the restricted SFTP drop
account on each destination, and proves each copy by reading it back and checking
every sha256. Only the small receipt stays on the hub. See README.md beside this
file for install, the drop design and the runbook.

Run by agent-hub-history-export.service (systemd user timer) as:
  uv run --no-project --with redis python -B agent-hub-history-export.py \
      --dest dtm-p1gen7 --mirror dtm-p1gen9

What it exports, from one PostgreSQL REPEATABLE READ snapshot plus a Redis dump:
  messages-pg/            agent_bus.messages (row_to_json, ordered by timestamp, id)
  presence-events-pg/     agent_bus.presence_events
  messages-redis-stream/  XRANGE agent_bus:messages (raw fields; lz4 bodies as stored)
  redis-state/            claims, resource events, direct/group channels, threads, tasks
  reconciliation.json     PG vs Redis message sets by UUID (they differ; both are kept)
Parts are gzip NDJSON. The bundle directory is named history-<first 16 hex of
sha256(SHA256SUMS)>, so the name is content-derived and never a date. An existing
directory of that name is never overwritten: sftp mkdir fails and the push stops.

Exit 1 when the required --dest fails. A failed --mirror is reported on the bus and
in the receipt but does not fail the run.
"""

from __future__ import annotations

import argparse
import base64
import gzip
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path

HOME = Path.home()
DEFAULT_KEY = HOME / ".ssh" / "agent-hub-drop_ed25519"
DEFAULT_DROP_USER = "agenthubdrop"
# Where the drop's chroot lives on the Windows hosts (Install-AgentHubDrop.ps1).
DROP_ROOT = "C:\\codedev\\agent-hub-artifacts\\incoming"
STATE = HOME / ".local" / "state" / "agent-hub-history-export"
CACHE = HOME / ".cache" / "agent-hub-history-export"
# Tests point this at a fake sftp; production uses the OpenSSH client on PATH.
SFTP = os.environ.get("AGENT_HUB_HISTORY_EXPORT_SFTP", "sftp")
PSQL = [
    "psql",
    "-X",
    "-w",
    "-q",
    "-h",
    "127.0.0.1",
    "-p",
    "5300",
    "-U",
    "postgres",
    "-d",
    "redis_backend",
    "-At",
    "-v",
    "ON_ERROR_STOP=1",
]
SIDE_CHANNELS = (
    "bus:claims:",
    "agent_bus:resource_events:",
    "bus:direct:",
    "bus:group:",
    "agent_bus:thread:",
    "bus:tasks:",
    "bus:escalations",
    "agent_bus:presence:",
    "agent_bus:sub:",
    "agent_bus:cloud_sync:",
)
# Bundle subdirectory -> (raw NDJSON file, lines per gzip part).
PARTS = {
    "messages-pg": ("messages.ndjson", 20000),
    "messages-redis-stream": ("redis-stream-messages.ndjson", 20000),
    "presence-events-pg": ("presence-events.ndjson", 30000),
}

PG_SQL = r"""
BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;
\o snapshot.tsv
SELECT 'snapshot_utc',
       to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"');
SELECT 'messages', count(*) FROM agent_bus.messages;
SELECT 'presence_events', count(*) FROM agent_bus.presence_events;
\o messages.ndjson
SELECT row_to_json(m)::text FROM agent_bus.messages m ORDER BY m.timestamp_utc, m.id;
\o presence-events.ndjson
SELECT row_to_json(p)::text FROM agent_bus.presence_events p ORDER BY p.id;
\o
COMMIT;
"""


def log(msg: str) -> None:
    print(f"agent-hub-history-export: {msg}", flush=True)


def dec(v):
    """Make a redis-py value JSON-safe; non-UTF-8 bytes become {"_b64": ...}."""
    if isinstance(v, bytes):
        try:
            return v.decode()
        except UnicodeDecodeError:
            return {"_b64": base64.b64encode(v).decode()}
    if isinstance(v, (list, tuple)):
        return [dec(x) for x in v]
    if isinstance(v, dict):
        return {dec(k): dec(x) for k, x in v.items()}
    return v


def export_pg(raw: Path) -> dict:
    (raw / "export.sql").write_text(PG_SQL)
    subprocess.run([*PSQL, "-f", "export.sql"], cwd=raw, check=True)
    (raw / "export.sql").unlink()
    snap = dict(
        line.split("|", 1) for line in (raw / "snapshot.tsv").read_text().splitlines()
    )
    return {
        "pg_snapshot_utc": snap["snapshot_utc"],
        "pg_messages": int(snap["messages"]),
        "pg_presence_events": int(snap["presence_events"]),
    }


def export_redis(raw: Path, r) -> dict:
    exported, skipped = Counter(), Counter()
    with open(raw / "redis-side-channels.ndjson", "w") as f:
        for k in sorted(r.scan_iter(count=2000)):
            key = k.decode()
            fam = ":".join(key.split(":")[:2])
            if not key.startswith(SIDE_CHANNELS):
                skipped[fam] += 1
                continue
            t = r.type(k).decode()
            if t == "stream":
                val = [[dec(i), dec(fl)] for i, fl in r.xrange(k)]
            elif t == "hash":
                val = dec(r.hgetall(k))
            elif t == "string":
                val = dec(r.get(k))
            elif t == "list":
                val = dec(r.lrange(k, 0, -1))
            elif t == "set":
                val = sorted(dec(list(r.smembers(k))))
            elif t == "zset":
                val = [[dec(m), s] for m, s in r.zrange(k, 0, -1, withscores=True)]
            else:
                continue
            f.write(
                json.dumps(
                    {"key": key, "type": t, "ttl": r.ttl(k), "value": val},
                    ensure_ascii=False,
                )
                + "\n"
            )
            exported[fam] += 1
    redis_uuids, n, last = set(), 0, "-"
    with open(raw / "redis-stream-messages.ndjson", "w") as f:
        while True:
            batch = r.xrange("agent_bus:messages", min=last, max="+", count=5000)
            if last != "-":
                batch = batch[1:]
            if not batch:
                break
            for i, fields in batch:
                rec = {
                    "stream_id": i.decode(),
                    "fields": {k.decode(): dec(v) for k, v in fields.items()},
                }
                f.write(json.dumps(rec, ensure_ascii=False) + "\n")
                redis_uuids.add(rec["fields"].get("id"))
                n += 1
            last = batch[-1][0].decode()
    with open(raw / "messages.ndjson") as f:
        pg_uuids = {json.loads(line)["id"] for line in f}
    recon = {
        "redis_stream_entries": n,
        "pg_rows": len(pg_uuids),
        "uuid_union": len(pg_uuids | redis_uuids),
        "only_in_redis": len(redis_uuids - pg_uuids),
        "only_in_pg": len(pg_uuids - redis_uuids),
    }
    (raw / "reconciliation.json").write_text(json.dumps(recon, indent=1) + "\n")
    (raw / "redis-side-channels.counts.json").write_text(
        json.dumps(
            {"exported_keys": exported, "skipped_keys": skipped},
            indent=1,
            sort_keys=True,
        )
        + "\n"
    )
    return recon


def check_window(before: int, snapshot: int, after: int) -> None:
    """Positive control: the snapshot count must lie between two live health readings.

    The hub only grows during the export, so a count outside [before, after] means
    the export read a different or truncated database, not the live hub.
    """
    if not before <= snapshot <= after:
        raise SystemExit(
            f"snapshot count {snapshot} outside health window [{before}, {after}]"
        )


def gz_parts(src: Path, dest_dir: Path, lines_per_part: int) -> int:
    dest_dir.mkdir(parents=True)
    total, part, out = 0, 0, None
    with open(src, "rb") as f:
        for i, line in enumerate(f):
            if i % lines_per_part == 0:
                if out:
                    out.close()
                # mtime=0 keeps the compressed bytes a pure function of the content.
                out = gzip.GzipFile(
                    dest_dir / f"part-{part:03d}.ndjson.gz", "wb", 6, mtime=0
                )
                part += 1
            out.write(line)
            total += 1
    if out:
        out.close()
    return total


def digest(path: Path) -> tuple[str, str, int]:
    s, m, n = hashlib.sha256(), hashlib.md5(), 0
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            s.update(chunk)
            m.update(chunk)
            n += len(chunk)
    return s.hexdigest(), m.hexdigest(), n


def build_bundle(raw: Path, bundle: Path, meta: dict) -> str:
    """Assemble the bundle, write MANIFEST.json and SHA256SUMS, and return its name."""
    bundle.mkdir(parents=True)
    counts = {
        name: gz_parts(raw / src, bundle / name, per)
        for name, (src, per) in PARTS.items()
    }
    (bundle / "redis-state").mkdir()
    for fn in ("redis-side-channels.ndjson", "redis-side-channels.counts.json"):
        shutil.copy2(raw / fn, bundle / "redis-state" / fn)
    for fn in ("reconciliation.json", "snapshot.tsv"):
        shutil.copy2(raw / fn, bundle / fn)
    files = []
    for p in sorted(bundle.rglob("*")):
        if p.is_file():
            sha, md5, size = digest(p)
            files.append(
                {
                    "path": p.relative_to(bundle).as_posix(),
                    "size": size,
                    "sha256": sha,
                    "md5": md5,
                }
            )
    meta = {
        **meta,
        "line_counts": counts,
        "files": files,
        "file_count": len(files),
        "total_bytes": sum(f["size"] for f in files),
        "body_encoding": "messages-pg bodies are plaintext; "
        "messages-redis-stream bodies flagged metadata._compressed=lz4 are base64 "
        "LZ4 blocks with a 4-byte LE size prefix "
        "(python: lz4.block.decompress(base64.b64decode(body)))",
    }
    (bundle / "MANIFEST.json").write_text(json.dumps(meta, indent=1) + "\n")
    sha, _, _ = digest(bundle / "MANIFEST.json")
    lines = [f"{f['sha256']}  {f['path']}" for f in files] + [f"{sha}  MANIFEST.json"]
    (bundle / "SHA256SUMS").write_text("\n".join(lines) + "\n")
    return "history-" + digest(bundle / "SHA256SUMS")[0][:16]


def verify_tree(root: Path) -> list[str]:
    """Check every SHA256SUMS line under root; return paths missing or differing."""
    bad = []
    for line in (root / "SHA256SUMS").read_text().splitlines():
        if not line.strip():
            continue
        want, rel = line.split("  ", 1)
        p = root / rel
        if not p.is_file() or digest(p)[0] != want:
            bad.append(rel)
    return bad


def sftp(
    host: str, batch: str, work: Path, key: Path, user: str
) -> subprocess.CompletedProcess:
    bf = work / "sftp.batch"
    bf.write_text(batch)
    opts = [
        "-i",
        str(key),
        "-o",
        "IdentitiesOnly=yes",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=20",
        "-o",
        "ServerAliveInterval=15",
    ]
    return subprocess.run(
        [SFTP, *opts, "-b", str(bf), f"{user}@{host}"],
        capture_output=True,
        text=True,
        timeout=3600,
        check=False,
    )


def push_and_verify(
    host: str, bundle: Path, name: str, work: Path, key: Path, user: str
) -> dict:
    """Upload the bundle into a new directory, read it back and verify every byte."""
    rel_files = sorted(
        p.relative_to(bundle).as_posix() for p in bundle.rglob("*") if p.is_file()
    )
    dirs = sorted({Path(f).parent.as_posix() for f in rel_files if "/" in f})
    # sftp -b stops at the first failing command: an existing name ends the push here.
    batch = [f'mkdir "{name}"'] + [f'mkdir "{name}/{d}"' for d in dirs]
    batch += [f'put "{bundle / f}" "{name}/{f}"' for f in rel_files]
    up = sftp(host, "\n".join(batch) + "\n", work, key, user)
    if up.returncode:
        return {
            "host": host,
            "ok": False,
            "stage": "upload",
            "rc": up.returncode,
            "stderr_tail": up.stderr.strip().splitlines()[-3:],
        }
    back = work / f"readback-{host}"
    back.mkdir()
    try:
        down = sftp(host, f'get -r "{name}" "{back}"\n', work, key, user)
        if down.returncode:
            return {
                "host": host,
                "ok": False,
                "stage": "readback",
                "rc": down.returncode,
                "stderr_tail": down.stderr.strip().splitlines()[-3:],
            }
        got = back / name
        got_files = sorted(
            p.relative_to(got).as_posix() for p in got.rglob("*") if p.is_file()
        )
        sums_equal = (got / "SHA256SUMS").is_file() and (
            got / "SHA256SUMS"
        ).read_bytes() == (bundle / "SHA256SUMS").read_bytes()
        bad = verify_tree(got) if sums_equal else ["SHA256SUMS"]
        ok = not bad and got_files == rel_files
        return {
            "host": host,
            "ok": ok,
            "stage": "verified" if ok else "readback-mismatch",
            "files": len(rel_files),
            "readback_files": len(got_files),
            "bad": bad[:10],
            "sha256sums_identical": sums_equal,
            "path": f"{DROP_ROOT}\\{name}",
        }
    finally:
        shutil.rmtree(back, ignore_errors=True)


def bus(body: str, ok: bool) -> None:
    topic, schema = ("status", "status") if ok else ("agent-hub-findings", "finding")
    subprocess.run(
        [
            "agent-bus",
            "send",
            "--from-agent",
            "agent-hub-history-export",
            "--to-agent",
            "all",
            "--topic",
            topic,
            "--schema",
            schema,
            "--thread-id",
            "machine-hygiene",
            "--tag",
            "repo:agent-hub",
            "--tag",
            "machine-hygiene",
            "--body",
            body,
            "--encoding",
            "compact",
        ],
        capture_output=True,
        timeout=60,
        check=False,
    )


def health() -> dict:
    return json.loads(
        subprocess.run(
            ["agent-bus", "health", "--encoding", "compact"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
    )


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--dest", required=True, help="required drop host (ssh name)")
    ap.add_argument(
        "--mirror", action="append", default=[], help="optional mirror drop host"
    )
    ap.add_argument(
        "--key", type=Path, default=DEFAULT_KEY, help="drop-only ed25519 key"
    )
    ap.add_argument("--drop-user", default=DEFAULT_DROP_USER)
    ap.add_argument(
        "--no-bus", action="store_true", help="do not post the result on the bus"
    )
    args = ap.parse_args(argv)
    import redis  # deferred so the pure helpers import without redis-py (tests, --help)

    CACHE.mkdir(parents=True, exist_ok=True)
    (STATE / "receipts").mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="run-", dir=CACHE))
    os.chmod(work, 0o700)
    try:
        raw, bundle = work / "raw", work / "bundle"
        raw.mkdir()
        before = health()
        meta = {
            "artifact": "agent-hub bus history export",
            "producer": "agent-hub-history-export (packaging/history-export)",
            "source_host": socket.gethostname(),
            "hub_build_version": before.get("build_version"),
        }
        meta.update(export_pg(raw))
        meta["reconciliation"] = export_redis(
            raw, redis.Redis(host="127.0.0.1", port=6380, db=0)
        )
        after = health()
        meta["health_pg_message_count_before_after"] = [
            before["pg_message_count"],
            after["pg_message_count"],
        ]
        check_window(
            before["pg_message_count"], meta["pg_messages"], after["pg_message_count"]
        )
        name = build_bundle(raw, bundle, meta)
        shutil.rmtree(raw)
        results = [
            push_and_verify(args.dest, bundle, name, work, args.key, args.drop_user)
        ]
        for m in args.mirror:
            try:
                results.append(
                    push_and_verify(m, bundle, name, work, args.key, args.drop_user)
                )
            except (subprocess.SubprocessError, OSError) as e:
                results.append(
                    {
                        "host": m,
                        "ok": False,
                        "stage": "exception",
                        "error": type(e).__name__,
                    }
                )
        receipt = {
            "name": name,
            # timezone.utc, not datetime.UTC: CI's Unit Tests python is 3.10.
            "completed_utc": datetime.now(timezone.utc).isoformat(timespec="seconds"),  # noqa: UP017
            "sha256sums_sha256": digest(bundle / "SHA256SUMS")[0],
            "manifest_sha256": digest(bundle / "MANIFEST.json")[0],
            "pg_snapshot_utc": meta["pg_snapshot_utc"],
            "pg_messages": meta["pg_messages"],
            "pg_presence_events": meta["pg_presence_events"],
            "reconciliation": meta["reconciliation"],
            "total_bytes": json.loads((bundle / "MANIFEST.json").read_text())[
                "total_bytes"
            ],
            "destinations": results,
        }
        rp = STATE / "receipts" / f"{name}.json"
        rp.write_text(json.dumps(receipt, indent=1) + "\n")
        for res in results:
            if res["ok"]:
                put = sftp(
                    res["host"],
                    f'put "{rp}" "{name}.receipt.json"\n',
                    work,
                    args.key,
                    args.drop_user,
                )
                res["receipt_uploaded"] = put.returncode == 0
        rp.write_text(json.dumps(receipt, indent=1) + "\n")
        required_ok = results[0]["ok"]
        summary = (
            f"{'STATUS' if required_ok else 'FINDING'}: "
            f"agent-hub history export {name}: "
            f"pg_messages={meta['pg_messages']} presence={meta['pg_presence_events']} "
            f"union={meta['reconciliation']['uuid_union']} "
            f"bytes={receipt['total_bytes']} "
            f"SHA256SUMS={receipt['sha256sums_sha256']}; "
            + "; ".join(f"{x['host']}={x['stage']}" for x in results)
            + f". Receipt {rp}"
        )
        log(summary)
        if not args.no_bus:
            bus(summary if required_ok else summary + "\nSEVERITY: high", required_ok)
        return 0 if required_ok else 1
    finally:
        shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
