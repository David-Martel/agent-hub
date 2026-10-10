#!/usr/bin/env python3
"""Import an agent-bus PostgreSQL CSV export into the agentbus cloud tier.

Reads the `\\copy` exports of `agent_bus.messages` (and optionally
`agent_bus.presence_events`) taken from an on-site hub, tags each row with the
hub it came from, drops anything that must never leave the site, and pushes the
rest to `POST /sync/push` (and `POST /sync/push-presence`) in batches of 500.

Safety properties:
- The bearer token is read from the AGENTBUS_TOKEN environment variable only;
  it never appears in argv, the state file, or any output.
- Rows whose id is listed in --exclude-ids, or whose text matches a credential
  pattern, are never sent. Each exclusion is logged by id and reason only.
- Idempotent: the cloud dedups on message id, so a re-run after a partial
  failure re-sends at most one batch. --state records the next row offset.
- --dry-run previews all rows without HTTP calls or state/report writes.
- Resume offsets are bound to the exact exports, exclusions and import settings;
  changed inputs and unbound nonzero legacy offsets are refused.
- Any row the CLOUD rejects (not merely a local skip/exclude, and not a
  same-id "conflict", which just means the row already exists with
  different content) is treated as a hard failure: the run exits non-zero
  and every rejected id/reason is written to --report, unless --allow-rejects
  is passed (in which case rejections are still reported, but the run still
  exits 0). `conflicts` (accepted-by-content, differs-from-existing) are
  counted and reported but never gate the exit code on their own.

Stdlib only, so it runs with `uv run --no-project python` on any fleet host.
"""

from __future__ import annotations

import argparse
import csv
import hashlib
import io
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from pathlib import Path

BATCH = 500
csv.field_size_limit(1 << 30)

# Credential-looking content. Matching is deliberately broad: a false positive
# costs one message in the off-site copy (it stays on-site), a false negative
# publishes a secret.
SECRET_PATTERNS = [
    (
        "github-token",
        re.compile(
            r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}|\bgithub_pat_[A-Za-z0-9_]{40,}"
        ),
    ),
    ("aws-key", re.compile(r"\bAKIA[0-9A-Z]{16}\b")),
    ("slack-token", re.compile(r"\bxox[abposr]-[A-Za-z0-9-]{10,}")),
    ("openai-anthropic-key", re.compile(r"\bsk-(?:ant-)?[A-Za-z0-9_-]{20,}")),
    ("google-api-key", re.compile(r"\bAIza[0-9A-Za-z_-]{35}\b")),
    ("private-key", re.compile(r"-----BEGIN [A-Z ]*PRIVATE KEY-----")),
    (
        "jwt",
        re.compile(
            r"\beyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}"
        ),
    ),
    ("bearer", re.compile(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{24,}")),
    ("url-credentials", re.compile(r"\b[a-z][a-z0-9+.-]*://[^\s:/@]+:[^\s@/]{6,}@")),
    (
        "assignment",
        re.compile(
            r"(?i)\b(?:password|passwd|secret|api[_-]?key|auth[_-]?token|access[_-]?token)\s*[=:]\s*['\"]?[^\s'\"<>{}$]{12,}"
        ),
    ),
]


def secret_reason(*texts: str) -> str | None:
    for text in texts:
        if not text:
            continue
        for name, pat in SECRET_PATTERNS:
            if pat.search(text):
                return name
    return None


def pg_ts_to_utc(value: str) -> str:
    """'2026-06-12 12:06:02.895291-04' -> '2026-06-12T16:06:02.895291Z'."""
    v = value.strip()
    if re.search(r"[+-]\d{2}$", v):
        v += ":00"
    dt = datetime.fromisoformat(v.replace(" ", "T", 1))
    if dt.tzinfo is None:
        raise ValueError(f"timestamp without offset: {value!r}")
    return dt.astimezone(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def pg_bool(value: str) -> bool:
    return value.strip().lower() in ("t", "true", "1")


def pg_json(value: str, default):
    value = (value or "").strip()
    if not value:
        return default
    return json.loads(value)


def message_item(row: dict, origin_hub: str, origin_host: str) -> dict:
    item = {
        "id": row["id"],
        "timestamp_utc": pg_ts_to_utc(row["timestamp_utc"]),
        "protocol_version": row["protocol_version"] or "1.0",
        "sender": row["sender"],
        "recipient": row["recipient"],
        "topic": row["topic"],
        "body": row["body"],
        "tags": pg_json(row["tags"], []),
        "priority": row["priority"] or "normal",
        "request_ack": pg_bool(row["request_ack"]),
        "metadata": pg_json(row["metadata"], {}),
        "origin_hub": origin_hub,
        "origin_host": origin_host,
    }
    if row.get("thread_id"):
        item["thread_id"] = row["thread_id"]
    if row.get("reply_to"):
        item["reply_to"] = row["reply_to"]
    return item


def presence_item(row: dict) -> dict:
    return {
        "origin_id": int(row["id"]),
        "timestamp_utc": pg_ts_to_utc(row["timestamp_utc"]),
        "protocol_version": row["protocol_version"] or "1.0",
        "agent": row["agent"],
        "status": row["status"],
        "session_id": row["session_id"] or None,
        "capabilities": pg_json(row["capabilities"], []),
        "metadata": pg_json(row["metadata"], {}),
        "ttl_seconds": int(row["ttl_seconds"]) if row["ttl_seconds"] else None,
    }


def post(url: str, token: str, payload: dict, retries: int = 5) -> dict:
    data = json.dumps(payload).encode()
    delay = 1.0
    for attempt in range(retries):
        req = urllib.request.Request(
            url,
            data=data,
            method="POST",
            headers={
                "Authorization": f"Bearer {token}",
                "Content-Type": "application/json",
            },
        )
        try:
            with urllib.request.urlopen(req, timeout=60) as resp:
                return json.loads(resp.read())
        except urllib.error.HTTPError as err:
            detail = err.read()[:300].decode(errors="replace")
            if err.code < 500 and err.code != 429:
                raise SystemExit(f"HTTP {err.code} from {url}: {detail}") from None
        except (urllib.error.URLError, TimeoutError) as err:
            detail = str(err)
        if attempt == retries - 1:
            raise SystemExit(f"giving up on {url} after {retries} attempts: {detail}")
        time.sleep(delay)
        delay = min(delay * 2, 30)
    raise AssertionError("unreachable")


def load_state(path: Path | None) -> dict:
    if path and path.exists():
        state = json.loads(path.read_text())
        if not isinstance(state, dict):
            raise SystemExit("resume state must be a JSON object")
        return state
    return {}


def save_state(path: Path | None, state: dict) -> None:
    if path:
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(state, indent=2))
        tmp.replace(path)


def bind_resume_state(state: dict, binding: dict) -> dict:
    """Refuse offsets whose immutable input identity is not established."""
    for key in ("messages_offset", "presence_offset"):
        offset = state.get(key, 0)
        if type(offset) is not int or offset < 0:
            raise SystemExit("resume offsets must be nonnegative integers")
    previous = state.get("binding")
    if previous is None:
        if state.get("messages_offset", 0) or state.get("presence_offset", 0):
            raise SystemExit("unbound nonzero legacy resume state refused")
    elif previous != binding:
        raise SystemExit("resume input or import settings changed; state refused")
    return state | {"binding": binding}


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument(
        "--url",
        required=True,
        help="cloud base URL, e.g. https://agentbus.dtmventures.com",
    )
    ap.add_argument(
        "--origin-hub", required=True, help="hub the export came from, e.g. asuspro13"
    )
    ap.add_argument("--origin-host", required=True, help="machine the export came from")
    ap.add_argument("--messages", type=Path, help="agent_bus.messages CSV export")
    ap.add_argument(
        "--presence", type=Path, help="agent_bus.presence_events CSV export"
    )
    ap.add_argument(
        "--exclude-ids", type=Path, help="file of message ids that must stay on-site"
    )
    ap.add_argument("--state", type=Path, help="resume-state JSON file")
    ap.add_argument("--report", type=Path, help="write a JSON summary here")
    ap.add_argument("--dry-run", action="store_true")
    ap.add_argument(
        "--allow-rejects",
        action="store_true",
        help="exit 0 even if the cloud rejected one or more rows (still reported)",
    )
    args = ap.parse_args()

    token = os.environ.get("AGENTBUS_TOKEN", "")
    if not args.dry_run and not token:
        raise SystemExit("AGENTBUS_TOKEN is not set")

    # Hash and parse the same captured bytes, rather than hashing then reopening
    # an export that could change before the rows are read.
    inputs = {
        name: path.read_bytes() if path else None
        for name, path in (
            ("messages", args.messages),
            ("presence", args.presence),
            ("exclude_ids", args.exclude_ids),
        )
    }
    excluded_ids: set[str] = set()
    if inputs["exclude_ids"] is not None:
        excluded_ids = {
            ln.split()[0]
            for ln in inputs["exclude_ids"].decode("utf-8-sig").splitlines()
            if ln.strip() and not ln.startswith("#")
        }

    settings = {
        "url": args.url.rstrip("/"),
        "origin_hub": args.origin_hub,
        "origin_host": args.origin_host,
        "allow_rejects": args.allow_rejects,
        "credential_filters": [
            [name, pattern.pattern, pattern.flags] for name, pattern in SECRET_PATTERNS
        ],
        "row_format_version": 1,
    }
    binding = {
        "version": 1,
        **{
            f"{name}_sha256": hashlib.sha256(data).hexdigest()
            if data is not None
            else None
            for name, data in inputs.items()
        },
        "settings_sha256": hashlib.sha256(
            json.dumps(settings, sort_keys=True).encode("utf-8")
        ).hexdigest(),
    }
    # A preview always examines the full inputs and never consumes a checkpoint.
    state = {} if args.dry_run else bind_resume_state(load_state(args.state), binding)
    for name in ("messages", "presence"):
        if inputs[name] is None and state.get(f"{name}_offset", 0):
            raise SystemExit("resume offset has no corresponding export")
    summary: dict = {
        "origin_hub": args.origin_hub,
        "dry_run": args.dry_run,
        "messages": {},
        "presence": {},
    }
    base = args.url.rstrip("/")

    if args.messages:
        rows = list(csv.DictReader(io.StringIO(inputs["messages"].decode("utf-8-sig"))))
        items, skipped = [], []
        for row in rows:
            if row["id"] in excluded_ids:
                skipped.append((row["id"], "exclude-list"))
                continue
            reason = secret_reason(row["body"], row["metadata"], row["tags"])
            if reason:
                skipped.append((row["id"], reason))
                continue
            items.append(message_item(row, args.origin_hub, args.origin_host))
        counts = {
            "rows": len(rows),
            "to_send": len(items),
            "skipped": len(skipped),
            "accepted": 0,
            "duplicates": 0,
            # A "conflict" is a same-id/client_msg_id row whose CONTENT
            # differs from what's already stored -- distinct from a
            # "rejection" (the cloud refused to store the row at all, e.g.
            # a validation failure). Counted and reported, but does not by
            # itself gate the exit code the way a rejection does (review
            # N15: "it never reads conflicts").
            "conflicts": 0,
            "rejected": 0,
        }
        start = state.get("messages_offset", 0)
        if start > len(items):
            raise SystemExit("messages resume offset exceeds filtered export")
        rejected_detail = []
        conflict_detail = []
        for off in range(start, len(items), BATCH):
            batch = items[off : off + BATCH]
            if not args.dry_run:
                res = post(
                    f"{base}/sync/push",
                    token,
                    {"origin_hub": args.origin_hub, "messages": batch},
                )
                counts["accepted"] += len(res.get("accepted", []))
                counts["duplicates"] += len(res.get("duplicates", []))
                counts["conflicts"] += len(res.get("conflicts", []))
                counts["rejected"] += len(res.get("rejected", []))
                conflict_detail += list(res.get("conflicts", []))
                rejected_detail += [
                    {"id": r.get("id"), "reason": r.get("reason")}
                    for r in res.get("rejected", [])
                ]
            if not args.dry_run:
                state["messages_offset"] = off + len(batch)
                save_state(args.state, state)
            print(f"messages {off + len(batch)}/{len(items)}", file=sys.stderr)
        summary["messages"] = counts | {
            "skipped_ids": [{"id": i, "reason": r} for i, r in skipped],
            "conflict_ids": conflict_detail,
            "rejected_ids": rejected_detail,
        }

    if args.presence:
        rows = list(csv.DictReader(io.StringIO(inputs["presence"].decode("utf-8-sig"))))
        items = [presence_item(r) for r in rows]
        counts = {
            "rows": len(rows),
            "to_send": len(items),
            "accepted": 0,
            "duplicates": 0,
            "rejected": 0,
        }
        start = state.get("presence_offset", 0)
        if start > len(items):
            raise SystemExit("presence resume offset exceeds export")
        presence_rejected_detail = []
        for off in range(start, len(items), BATCH):
            batch = items[off : off + BATCH]
            if not args.dry_run:
                res = post(
                    f"{base}/sync/push-presence",
                    token,
                    {
                        "origin_hub": args.origin_hub,
                        "origin_host": args.origin_host,
                        "events": batch,
                    },
                )
                counts["accepted"] += int(res.get("accepted", 0))
                counts["duplicates"] += int(res.get("duplicates", 0))
                # re-review N15: the importer used to ignore presence
                # `rejected` entirely -- a malformed/oversized presence
                # event would silently vanish with no signal at all.
                rejected_events = res.get("rejected", [])
                counts["rejected"] += len(rejected_events)
                presence_rejected_detail += [
                    {"origin_id": r.get("origin_id"), "reason": r.get("reason")}
                    for r in rejected_events
                ]
            if not args.dry_run:
                state["presence_offset"] = off + len(batch)
                save_state(args.state, state)
            print(f"presence {off + len(batch)}/{len(items)}", file=sys.stderr)
        summary["presence"] = counts | {"rejected_ids": presence_rejected_detail}

    total_rejected = summary["messages"].get("rejected", 0) + summary["presence"].get(
        "rejected", 0
    )
    summary["total_rejected"] = total_rejected
    summary["allow_rejects"] = args.allow_rejects

    out = json.dumps(summary, indent=2)
    if args.report and not args.dry_run:
        args.report.write_text(out)
    brief = {
        k: (
            {kk: vv for kk, vv in v.items() if not kk.endswith("_ids")}
            if isinstance(v, dict)
            else v
        )
        for k, v in summary.items()
    }
    print(json.dumps(brief, indent=2))

    # re-review N15: the importer used to always exit 0, so a partially
    # rejected import looked identical to a fully successful one to any
    # caller/CI job that only checks the exit code. A rejection is a row
    # the CLOUD refused outright (a validation failure, an oversized body,
    # ...) -- distinct from a "conflict" (accepted, but the id already
    # existed with different content), which is expected on a re-run and
    # never gates the exit code.
    if total_rejected > 0 and not args.allow_rejects:
        print(
            f"error: {total_rejected} row(s) were rejected by the cloud tier "
            "(see rejected_ids in --report, or pass --allow-rejects)",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
