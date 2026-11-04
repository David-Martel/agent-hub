"""Back up full agent-bus history to encrypted, owner-only private Google Drive.

The existing daily service uses --drive, as authorized by the operator's
2026-11-03 backup migration directive. Full ciphertext and completion-receipt
readback are required. Raw export, bundle, tar and ciphertext stay in the owned
spool pending independent decrypt/restore qualification. No Windows host is a
required destination. See README.md, "Private Drive backup", for installation,
fixed destination, isolated tests and the reviewed activation gate.

Run the reviewed helper through the existing flock with:
  uv run --offline --no-project --with redis python -B \
      agent-hub-history-export.py --drive

What it exports, from one PostgreSQL REPEATABLE READ snapshot plus a Redis dump:
  messages-pg/            agent_bus.messages (row_to_json, ordered by timestamp, id)
  presence-events-pg/     agent_bus.presence_events
  messages-redis-stream/  XRANGE agent_bus:messages (raw fields; lz4 bodies as stored)
  redis-state/            claims, resource events, direct/group channels, threads, tasks
  reconciliation.json     PG vs Redis message sets by UUID (they differ; both are kept)
Parts are gzip NDJSON. The bundle directory is named history-<first 16 hex of
sha256(SHA256SUMS)>, so the name is content-derived and never a date.

Legacy explicit --dest/--mirror remains for historical restricted SFTP drops.
That mode refuses an existing remote directory, reads back all files, retains
only receipts locally and fails when its required destination fails. Those
legacy transport/retention semantics do not describe the current Drive service.
"""

from __future__ import annotations

import argparse
import base64
import configparser
import gzip
import hashlib
import json
import os
import re
import selectors
import shutil
import socket
import subprocess
import stat
import sys
import tarfile
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request
from collections import Counter
from datetime import datetime, timezone
from pathlib import Path, PurePosixPath

HOME = Path.home()
DEFAULT_KEY = HOME / ".ssh" / "agent-hub-drop_ed25519"
DEFAULT_DROP_USER = "agenthubdrop"
# Where the drop's chroot lives on the Windows hosts (Install-AgentHubDrop.ps1).
DROP_ROOT = "C:\\codedev\\agent-hub-artifacts\\incoming"
STATE = HOME / ".local" / "state" / "agent-hub-history-export"
CACHE = HOME / ".cache" / "agent-hub-history-export"
# Fixed private destination qualified by the operator; never inherit another remote.
DRIVE_ROOT = Path("/srv/vigil-migration/asuspro13")
DRIVE_CONFIG = DRIVE_ROOT / "rclone.conf"
DRIVE_RECIPIENT = DRIVE_ROOT / "age-recipient.txt"
DRIVE_FOLDER = "1mmL8o1upp4ty5zuaghn0YOfv40vsHzyt"
DRIVE_OWNER_SHA = "4b08374a88c3bc5243fa76e05332bfb0dc126bf78de8d471b603765f544a9030"
DRIVE_RECIPIENT_SHA = "b7965828eac40a779a8732d11705dc8c17e88bca15eb0282f7bc4f9cb90415e9"
DRIVE_RESERVE = 100 * 1024**3
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


def upload_receipts(
    results: list[dict], rp: Path, name: str, work: Path, key: Path, user: str
) -> None:
    """Upload the receipt to every verified destination, never raising.

    A timeout or OS error on one destination (typically an optional mirror) is
    recorded as ``receipt_uploaded: false`` with the exception type, so the other
    destinations keep their results and the run still writes its summary.
    """
    for res in (r for r in results if r["ok"]):
        try:
            put = sftp(
                res["host"], f'put "{rp}" "{name}.receipt.json"\n', work, key, user
            )
        except (subprocess.SubprocessError, OSError) as e:
            res["receipt_uploaded"] = False
            res["receipt_error"] = type(e).__name__
        else:
            res["receipt_uploaded"] = put.returncode == 0


class DriveHold(Exception):
    """A fixed, payload-free reason to retain the owned backup for retry."""


def drive_environment() -> dict:
    """Remove rclone overrides; every command names the qualified config explicitly."""
    return {k: v for k, v in os.environ.items() if not k.startswith("RCLONE_")}


def drive_run(args: list[str], timeout: int = 120) -> bytes:
    try:
        result = subprocess.run(
            args,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            timeout=timeout,
            env=drive_environment(),
            check=False,
        )
    except subprocess.TimeoutExpired:
        raise DriveHold("command_timeout") from None
    if result.returncode:
        raise DriveHold("command_failed_exit_" + str(result.returncode))
    return result.stdout


def drive_json(path: Path) -> dict:
    if path.is_symlink() or not path.is_file() or path.stat().st_size > 16 * 1024**2:
        raise DriveHold("regular_metadata_required")
    value = json.loads(path.read_text())
    if not isinstance(value, dict):
        raise DriveHold("metadata_object_required")
    return value


def drive_atomic(path: Path, value: dict) -> None:
    temporary = path.with_name(path.name + ".pending")
    data = (json.dumps(value, indent=2, sort_keys=True) + "\n").encode()
    with temporary.open("xb") as stream:
        stream.write(data)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)
    if os.name == "posix":
        fd = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(fd)
        finally:
            os.close(fd)


def drive_hash(path: Path) -> dict:
    if path.is_symlink() or not path.is_file():
        raise DriveHold("regular_backup_file_required")
    before = path.stat()
    sha, md5, size = digest(path)
    after = path.stat()
    if (before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) != (
        after.st_ino,
        after.st_size,
        after.st_mtime_ns,
        after.st_ctime_ns,
    ):
        raise DriveHold("backup_changed_while_reading")
    return {"sha256": sha, "md5": md5, "bytes": size}


def validate_drive_bundle(root: Path) -> dict:
    """Verify the exact census and parsed NDJSON; see README Drive restore."""
    if root.is_symlink() or not root.is_dir() or (root / "SHA256SUMS").is_symlink():
        raise DriveHold("nonregular_bundle_root")
    sums = {}
    for line in (root / "SHA256SUMS").read_text().splitlines():
        sha, rel = line.split("  ", 1)
        parts = PurePosixPath(rel)
        if (
            not re.fullmatch(r"[0-9a-f]{64}", sha)
            or parts.is_absolute()
            or ".." in parts.parts
            or "\\" in rel
            or parts.as_posix() != rel
            or rel in sums
            or not rel
            or ":" in rel
        ):
            raise DriveHold("unsafe_or_duplicate_bundle_member")
        sums[rel] = sha
    actual = set()
    for path in root.rglob("*"):
        if path.is_symlink() or not (path.is_dir() or path.is_file()):
            raise DriveHold("nonregular_bundle_member")
        if path.is_file():
            actual.add(path.relative_to(root).as_posix())
    if actual != set(sums) | {"SHA256SUMS"} or "MANIFEST.json" not in sums:
        raise DriveHold("bundle_census_mismatch")
    for rel, sha in sums.items():
        if drive_hash(root / rel)["sha256"] != sha:
            raise DriveHold("bundle_hash_mismatch")
    manifest = drive_json(root / "MANIFEST.json")
    records = manifest["files"]
    if not isinstance(records, list) or len(records) != manifest["file_count"]:
        raise DriveHold("manifest_census_mismatch")
    expected = {row["path"] for row in records}
    if len(expected) != len(records) or expected | {"MANIFEST.json"} != set(sums):
        raise DriveHold("manifest_census_mismatch")
    if sum(row["size"] for row in records) != manifest["total_bytes"]:
        raise DriveHold("manifest_size_mismatch")
    for row in records:
        info = drive_hash(root / row["path"])
        if info != {"sha256": row["sha256"], "md5": row["md5"], "bytes": row["size"]}:
            raise DriveHold("manifest_file_mismatch")
    for directory, count in manifest["line_counts"].items():
        if directory not in PARTS or type(count) is not int or count < 0:
            raise DriveHold("manifest_count_invalid")
        observed = 0
        for path in sorted((root / directory).glob("*.ndjson.gz")):
            with gzip.open(path, "rt", encoding="utf-8") as stream:
                for line in stream:
                    if not isinstance(json.loads(line), dict):
                        raise DriveHold("ndjson_object_required")
                    observed += 1
        if observed != count:
            raise DriveHold("ndjson_count_mismatch")
    if set(manifest["line_counts"]) != set(PARTS):
        raise DriveHold("manifest_count_missing")
    snapshot_rows = [
        line.split("|", 1) for line in (root / "snapshot.tsv").read_text().splitlines()
    ]
    if (
        any(len(row) != 2 for row in snapshot_rows)
        or len(snapshot_rows) != 3
        or {row[0] for row in snapshot_rows}
        != {"snapshot_utc", "messages", "presence_events"}
    ):
        raise DriveHold("export_snapshot_fields_invalid")
    snapshot = dict(snapshot_rows)
    try:
        snapshot_messages = int(snapshot["messages"])
        snapshot_presence = int(snapshot["presence_events"])
    except ValueError:
        raise DriveHold("export_snapshot_fields_invalid") from None
    if (
        type(manifest.get("pg_messages")) is not int
        or type(manifest.get("pg_presence_events")) is not int
        or snapshot_messages != manifest["pg_messages"]
        or snapshot_presence != manifest["pg_presence_events"]
        or snapshot_messages != manifest["line_counts"]["messages-pg"]
        or snapshot_presence != manifest["line_counts"]["presence-events-pg"]
        or snapshot["snapshot_utc"] != manifest.get("pg_snapshot_utc")
    ):
        raise DriveHold("export_snapshot_count_mismatch")
    with (root / "redis-state/redis-side-channels.ndjson").open() as stream:
        for line in stream:
            if not isinstance(json.loads(line), dict):
                raise DriveHold("redis_state_object_required")
    return manifest


class DriveStore:
    """Owner-only immutable Drive writes and complete readback; no private age key."""

    def __init__(self) -> None:
        qualification = drive_json(DRIVE_ROOT / "transport-qualification.json")
        self.owner = qualification["account"]
        if (
            qualification.get("status") != "PASS"
            or not qualification.get("owner_private")
            or qualification.get("folder_id") != DRIVE_FOLDER
            or hashlib.sha256(self.owner.encode()).hexdigest() != DRIVE_OWNER_SHA
        ):
            raise DriveHold("unqualified_private_destination")

    def rc(self, *args: str, timeout: int = 120) -> bytes:
        return drive_run(
            ["/usr/bin/rclone", "--config", str(DRIVE_CONFIG), *args], timeout
        )

    def api(self, path: str) -> dict:
        if DRIVE_CONFIG.is_symlink():
            raise DriveHold("dedicated_config_required")
        info = DRIVE_CONFIG.stat()
        if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) != 0o600:
            raise DriveHold("dedicated_config_permissions")
        config = configparser.ConfigParser(interpolation=None)
        config.read(DRIVE_CONFIG)
        remote = config["migration-drive"]
        if (
            remote.get("type") != "drive"
            or remote.get("root_folder_id") != DRIVE_FOLDER
        ):
            raise DriveHold("dedicated_folder_required")
        token = json.loads(remote["token"])["access_token"]
        request = urllib.request.Request(
            "https://www.googleapis.com/drive/v3/" + path,
            headers={"Authorization": "Bearer " + token},
        )
        for attempt in range(3):
            try:
                with urllib.request.urlopen(request, timeout=45) as response:
                    return json.load(response)
            except urllib.error.HTTPError as error:
                if error.code not in (429, 500, 502, 503, 504):
                    break
            except (OSError, ValueError):
                if attempt == 2:
                    raise DriveHold("official_drive_read_failed") from None
            if attempt < 2:
                time.sleep(2**attempt)
        raise DriveHold("official_drive_read_failed") from None

    @staticmethod
    def check_private(obj: dict, permissions: dict, owner: str) -> None:
        if (
            obj.get("trashed")
            or obj.get("owners") != [{"emailAddress": owner}]
            or permissions.get("nextPageToken")
            or permissions.get("permissions")
            != [{"type": "user", "role": "owner", "emailAddress": owner}]
        ):
            raise DriveHold("private_owner_only_proof_failed")

    def private(self, oid: str) -> dict:
        fields = (
            "id,name,mimeType,parents,size,md5Checksum,sha256Checksum,"
            "trashed,webViewLink,owners(emailAddress)"
        )
        obj = self.api("files/" + oid + "?fields=" + fields)
        permissions = self.api(
            "files/"
            + oid
            + "/permissions?pageSize=100&fields=nextPageToken,"
            + "permissions(type,role,emailAddress)"
        )
        self.check_private(obj, permissions, self.owner)
        return obj

    def account(self) -> None:
        self.rc(
            "about",
            "migration-drive:",
            "--json",
            "--retries",
            "2",
            "--low-level-retries",
            "5",
        )
        if (
            self.api("about?fields=user(emailAddress)")["user"]["emailAddress"]
            != self.owner
        ):
            raise DriveHold("private_account_mismatch")
        if (
            self.private(DRIVE_FOLDER)["mimeType"]
            != "application/vnd.google-apps.folder"
        ):
            raise DriveHold("private_folder_type_mismatch")

    def readback(self, name: str, expected: dict) -> dict:
        args = [
            "/usr/bin/rclone",
            "--config",
            str(DRIVE_CONFIG),
            "cat",
            "migration-drive:" + name,
            "--retries",
            "2",
            "--low-level-retries",
            "5",
            "--timeout",
            "60s",
            "--contimeout",
            "30s",
        ]
        process = subprocess.Popen(
            args,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            env=drive_environment(),
        )
        selector = selectors.DefaultSelector()
        selector.register(process.stdout, selectors.EVENT_READ)
        deadline, sha, size = time.monotonic() + 3600, hashlib.sha256(), 0
        try:
            while True:
                if time.monotonic() >= deadline:
                    raise DriveHold("remote_readback_timeout")
                if not selector.select(timeout=10) and process.poll() is None:
                    continue
                chunk = os.read(process.stdout.fileno(), 1024**2)
                if not chunk:
                    break
                sha.update(chunk)
                size += len(chunk)
            if (
                process.wait(timeout=30)
                or size != expected["bytes"]
                or sha.hexdigest() != expected["sha256"]
            ):
                raise DriveHold("full_remote_stream_mismatch")
        finally:
            selector.close()
            process.stdout.close()
            if process.poll() is None:
                process.kill()
                process.wait()
        return {
            "bytes": size,
            "sha256": sha.hexdigest(),
            "verified_utc": datetime.now(timezone.utc).isoformat(),
        }

    def put(self, path: Path, name: str, expected: dict) -> dict:
        if not re.fullmatch(r"[a-zA-Z0-9_.-]+", name):
            raise DriveHold("unsafe_remote_name")
        query = "'%s' in parents and name='%s' and trashed=false" % (DRIVE_FOLDER, name)

        def find():
            result = self.api(
                "files?q="
                + urllib.parse.quote(query, safe="")
                + "&pageSize=100&fields=nextPageToken,files(id)"
            )
            if result.get("nextPageToken") or len(result.get("files", [])) > 1:
                raise DriveHold("ambiguous_remote_name")
            return result.get("files", [])

        found = find()
        if not found:
            self.rc(
                "copyto",
                str(path),
                "migration-drive:" + name,
                "--immutable",
                "--retries",
                "2",
                "--low-level-retries",
                "5",
                "--drive-stop-on-upload-limit",
                "--timeout",
                "60s",
                "--contimeout",
                "30s",
                timeout=3600,
            )
            found = find()
        if len(found) != 1:
            raise DriveHold("remote_object_missing")
        obj = self.private(found[0]["id"])
        if (
            obj.get("parents") != [DRIVE_FOLDER]
            or int(obj.get("size", -1)) != expected["bytes"]
            or obj.get("md5Checksum") != expected["md5"]
            or (
                obj.get("sha256Checksum")
                and obj["sha256Checksum"] != expected["sha256"]
            )
        ):
            raise DriveHold("remote_object_binding_mismatch")
        return {"object": obj, "full_readback": self.readback(name, expected)}


def close_drive_job(job: Path, name: str) -> dict:
    """Seal one generated bundle, keeping all plaintext until independent restore."""
    if job.is_symlink() or not re.fullmatch(r"history-[0-9a-f]{16}", name):
        raise DriveHold("unsafe_job_identity")
    binding_path = job / "binding.json"
    if binding_path.exists():
        binding = drive_json(binding_path)
        if (
            binding.get("schema") != "agent-hub.history-drive-job/v1"
            or binding["name"] != name
            or binding["recipient_sha256"] != DRIVE_RECIPIENT_SHA
            or binding["cipher_file"] != binding["cipher"]["sha256"] + ".age"
            or not re.fullmatch(r"[0-9a-f]{64}\.age", binding["cipher_file"])
        ):
            raise DriveHold("closed_binding_changed")
        if drive_hash(job / binding["cipher_file"]) != binding["cipher"]:
            raise DriveHold("closed_cipher_changed")
        if drive_hash(job / "archive.tar") != binding["plain"]:
            raise DriveHold("closed_plaintext_changed")
        if (
            digest(job / "bundle/MANIFEST.json")[0] != binding["manifest_sha256"]
            or digest(job / "bundle/SHA256SUMS")[0] != binding["sha256sums_sha256"]
        ):
            raise DriveHold("closed_manifest_changed")
        return binding
    bundle = job / "bundle"
    manifest = validate_drive_bundle(bundle)
    if "history-" + digest(bundle / "SHA256SUMS")[0][:16] != name:
        raise DriveHold("history_name_mismatch")
    archive = job / "archive.tar"
    if (
        archive.exists()
        or (job / "ciphertext.pending").exists()
        or list(job.glob("*.age"))
    ):
        raise DriveHold("unbound_partial_preserved")
    needed = sum(p.stat().st_size for p in bundle.rglob("*") if p.is_file())
    if shutil.disk_usage(job).free < DRIVE_RESERVE + needed * 3 + 1024**2:
        raise DriveHold("backup_100GiB_reserve")
    with tarfile.open(archive, "x") as output:
        for path in sorted(p for p in bundle.rglob("*") if p.is_file()):
            header = tarfile.TarInfo(path.relative_to(bundle).as_posix())
            header.size, header.mode = path.stat().st_size, 0o600
            with path.open("rb") as stream:
                output.addfile(header, stream)
    with archive.open("r+b") as stream:
        os.fsync(stream.fileno())
    plain = drive_hash(archive)
    if digest(DRIVE_RECIPIENT)[0] != DRIVE_RECIPIENT_SHA:
        raise DriveHold("public_recipient_changed")
    pending = job / "ciphertext.pending"
    drive_run(
        [
            "/usr/bin/age",
            "--encrypt",
            "--recipients-file",
            str(DRIVE_RECIPIENT),
            "--output",
            str(pending),
            str(archive),
        ],
        timeout=3600,
    )
    with pending.open("r+b") as stream:
        os.fsync(stream.fileno())
    if drive_hash(archive) != plain:
        raise DriveHold("plaintext_changed_during_encryption")
    cipher = drive_hash(pending)
    cipher_file = cipher["sha256"] + ".age"
    if (job / cipher_file).exists():
        raise DriveHold("closed_cipher_collision")
    pending.rename(job / cipher_file)
    binding = {
        "schema": "agent-hub.history-drive-job/v1",
        "name": name,
        "plain": plain,
        "cipher": cipher,
        "cipher_file": cipher_file,
        "recipient_sha256": DRIVE_RECIPIENT_SHA,
        "manifest_sha256": digest(bundle / "MANIFEST.json")[0],
        "sha256sums_sha256": digest(bundle / "SHA256SUMS")[0],
        "pg_snapshot_utc": manifest.get("pg_snapshot_utc"),
        "line_counts": manifest["line_counts"],
    }
    drive_atomic(binding_path, binding)
    return binding


def publish_drive_job(job: Path, store: DriveStore) -> dict:
    binding = drive_json(job / "binding.json")
    binding = close_drive_job(job, binding["name"])
    store.account()
    prefix = "agent-hub." + binding["name"] + "." + binding["cipher"]["sha256"]
    proof = store.put(
        job / binding["cipher_file"], prefix + ".tar.age", binding["cipher"]
    )
    receipt_path = job / "cloud-receipt.json"
    if receipt_path.exists():
        receipt = drive_json(receipt_path)
        if (
            receipt["binding"] != binding
            or receipt["cipher_proof"]["object"]["id"] != proof["object"]["id"]
        ):
            raise DriveHold("existing_receipt_binding_changed")
    else:
        receipt = {
            "schema": "agent-hub.history-drive-receipt/v1",
            "status": "REMOTE_VERIFIED",
            "binding": binding,
            "cipher_proof": proof,
            "source_host": socket.gethostname(),
            "source_private_age_key": False,
            "plaintext_retained_for_independent_restore": True,
        }
        drive_atomic(receipt_path, receipt)
    receipt_hash = drive_hash(receipt_path)
    receipt_proof = store.put(receipt_path, prefix + ".receipt.json", receipt_hash)
    verified = {
        **receipt,
        "completion_receipt": {"hash": receipt_hash, "proof": receipt_proof},
    }
    drive_atomic(job / "verified.json", verified)
    return verified


def drive_backup(no_bus: bool = False) -> int:
    """Resume one closed job or capture once; all failed/plaintext material is kept."""
    spool = STATE / "drive-spool"
    spool.mkdir(parents=True, exist_ok=True, mode=0o700)
    os.chmod(spool, 0o700)
    job = None
    try:
        if shutil.disk_usage(spool).free < DRIVE_RESERVE:
            raise DriveHold("backup_100GiB_reserve")
        for candidate in sorted(spool.iterdir()):
            if candidate.is_symlink() or not candidate.is_dir():
                raise DriveHold("unknown_spool_entry_preserved")
            if (candidate / "verified.json").exists():
                complete = drive_json(candidate / "verified.json")
                if (
                    complete.get("status") != "REMOTE_VERIFIED"
                    or complete.get("binding") != drive_json(candidate / "binding.json")
                    or not complete.get("completion_receipt")
                ):
                    raise DriveHold("completed_metadata_changed")
            else:
                job = candidate
                break
        if job is None:
            import redis

            job = spool / "pending"
            job.mkdir(mode=0o700)
            raw, bundle = job / "raw", job / "bundle"
            raw.mkdir()
            before = health()
            meta = {
                "artifact": "agent-hub bus history export",
                "source_host": socket.gethostname(),
                "producer": "agent-hub-history-export (packaging/history-export)",
                "hub_build_version": before.get("build_version"),
            }
            meta.update(export_pg(raw))
            meta["reconciliation"] = export_redis(
                raw, redis.Redis(host="127.0.0.1", port=6380, db=0)
            )
            after = health()
            check_window(
                before["pg_message_count"],
                meta["pg_messages"],
                after["pg_message_count"],
            )
            meta["health_pg_message_count_before_after"] = [
                before["pg_message_count"],
                after["pg_message_count"],
            ]
            name = build_bundle(raw, bundle, meta)
        elif (job / "binding.json").exists():
            name = drive_json(job / "binding.json")["name"]
        elif (job / "bundle/SHA256SUMS").is_file():
            name = "history-" + digest(job / "bundle/SHA256SUMS")[0][:16]
        else:
            raise DriveHold("unfinished_export_preserved")
        close_drive_job(job, name)
        if job.name == "pending":
            target = spool / name
            if target.exists():
                raise DriveHold("existing_job_preserved")
            job.rename(target)
            job = target
        verified = publish_drive_job(job, DriveStore())
        (STATE / "receipts").mkdir(exist_ok=True)
        drive_atomic(STATE / "receipts" / (name + ".drive.json"), verified)
        summary = (
            "STATUS: agent-hub encrypted Drive history "
            + name
            + ": REMOTE_VERIFIED; plaintext and cipher retained; receipt "
            + str(job / "verified.json")
        )
        drive_atomic(
            STATE / "last-drive-result.json",
            {
                "status": "REMOTE_VERIFIED",
                "name": name,
                "observed_utc": datetime.now(timezone.utc).isoformat(),
                "job": str(job),
            },
        )
        log(summary)
        if not no_bus:
            try:
                bus(summary, True)
            except (subprocess.SubprocessError, OSError):
                log("Drive backup verified; optional bus notice failed")
        return 0
    except (
        DriveHold,
        OSError,
        ValueError,
        KeyError,
        TypeError,
        subprocess.SubprocessError,
    ) as error:
        reason = (
            str(error)
            if isinstance(error, DriveHold)
            else "unexpected_" + type(error).__name__
        )
        drive_atomic(
            STATE / "last-drive-result.json",
            {
                "status": "HELD",
                "reason": reason,
                "observed_utc": datetime.now(timezone.utc).isoformat(),
                "job": str(job) if job else None,
            },
        )
        log("Drive backup HELD " + reason + "; owned spool retained")
        if not no_bus:
            try:
                bus(
                    "FINDING: agent-hub encrypted Drive history HELD "
                    + reason
                    + "; owned spool retained\nSEVERITY: high",
                    False,
                )
            except (subprocess.SubprocessError, OSError):
                log("Drive backup held; optional bus notice failed")
        return 1


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    target = ap.add_mutually_exclusive_group(required=True)
    target.add_argument("--dest", help="legacy required drop host (ssh name)")
    target.add_argument(
        "--drive",
        action="store_true",
        help="encrypted qualified private Google Drive backup",
    )
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
    if args.drive:
        if args.mirror:
            ap.error("--drive cannot use SFTP mirrors")
        return drive_backup(args.no_bus)
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
        upload_receipts(results, rp, name, work, args.key, args.drop_user)
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
