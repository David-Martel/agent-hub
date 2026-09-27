"""Unit tests for import_pg_export.py.

Stdlib + pytest only (no network, no real cloud). Run with:

    uv run --no-project --with pytest pytest test_import_pg_export.py

These tests exercise:
- pg_ts_to_utc's timestamp conversion (the on-site Postgres export format ->
  the cloud's canonical ISO-8601 UTC format).
- secret_reason's credential-pattern screen.
- message_item/presence_item's row shaping.
- The re-review N15 importer fixes: conflicts are read and counted,
  presence rejections are read and counted, and the run exits non-zero on
  any cloud-side rejection unless --allow-rejects is passed -- verified via
  main() end-to-end with `post()` monkeypatched (no real HTTP call), so
  these are true regression tests for the behavior the coordinator asked
  for, not just unit tests of helper functions.
"""

from __future__ import annotations

import csv
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import import_pg_export as ipe  # noqa: E402


# --- pg_ts_to_utc -----------------------------------------------------------


def test_pg_ts_to_utc_converts_offset_timezone():
    assert (
        ipe.pg_ts_to_utc("2026-06-12 12:06:02.895291-04")
        == "2026-06-12T16:06:02.895291Z"
    )


def test_pg_ts_to_utc_handles_utc_offset_zero():
    assert (
        ipe.pg_ts_to_utc("2026-06-12 12:06:02.000000+00")
        == "2026-06-12T12:06:02.000000Z"
    )


def test_pg_ts_to_utc_rejects_naive_timestamp():
    import pytest

    with pytest.raises(ValueError):
        ipe.pg_ts_to_utc("2026-06-12 12:06:02.895291")


# --- secret_reason -----------------------------------------------------------


def test_secret_reason_detects_github_token():
    assert ipe.secret_reason("here is ghp_" + "a" * 36) == "github-token"


def test_secret_reason_detects_aws_key():
    assert ipe.secret_reason("AKIA" + "A" * 16) == "aws-key"


def test_secret_reason_none_for_ordinary_text():
    assert ipe.secret_reason("STATUS: build passed, 12 tests green") is None


# --- message_item / presence_item -------------------------------------------


def test_message_item_shapes_row_and_carries_origin():
    row = {
        "id": "m1",
        "timestamp_utc": "2026-06-12 12:06:02.895291-04",
        "protocol_version": "1.0",
        "sender": "claude",
        "recipient": "codex",
        "topic": "status",
        "body": "hi",
        "tags": "[]",
        "priority": "normal",
        "request_ack": "f",
        "metadata": "{}",
        "thread_id": "",
        "reply_to": "",
    }
    item = ipe.message_item(row, origin_hub="asuspro13", origin_host="asuspro13.local")
    assert item["id"] == "m1"
    assert item["timestamp_utc"] == "2026-06-12T16:06:02.895291Z"
    assert item["origin_hub"] == "asuspro13"
    assert item["origin_host"] == "asuspro13.local"
    # Empty thread_id/reply_to are omitted, not sent as "".
    assert "thread_id" not in item
    assert "reply_to" not in item


def test_presence_item_coerces_types():
    row = {
        "id": "42",
        "timestamp_utc": "2026-06-12 12:06:02.895291-04",
        "protocol_version": "1.0",
        "agent": "claude",
        "status": "online",
        "session_id": "sess1",
        "capabilities": "[]",
        "metadata": "{}",
        "ttl_seconds": "180",
    }
    item = ipe.presence_item(row)
    assert item["origin_id"] == 42
    assert item["ttl_seconds"] == 180


# --- main(): conflicts/rejections/--allow-rejects (re-review N15) ----------


def _write_messages_csv(path: Path, n: int = 1) -> None:
    with path.open("w", newline="") as f:
        writer = csv.DictWriter(
            f,
            fieldnames=[
                "id",
                "timestamp_utc",
                "protocol_version",
                "sender",
                "recipient",
                "topic",
                "body",
                "tags",
                "priority",
                "request_ack",
                "metadata",
                "thread_id",
                "reply_to",
            ],
        )
        writer.writeheader()
        for i in range(n):
            writer.writerow(
                {
                    "id": f"m{i}",
                    "timestamp_utc": "2026-06-12 12:06:02.895291-04",
                    "protocol_version": "1.0",
                    "sender": "claude",
                    "recipient": "codex",
                    "topic": "status",
                    "body": f"message {i}",
                    "tags": "[]",
                    "priority": "normal",
                    "request_ack": "f",
                    "metadata": "{}",
                    "thread_id": "",
                    "reply_to": "",
                }
            )


def _write_presence_csv(path: Path, n: int = 1) -> None:
    with path.open("w", newline="") as f:
        writer = csv.DictWriter(
            f,
            fieldnames=[
                "id",
                "timestamp_utc",
                "protocol_version",
                "agent",
                "status",
                "session_id",
                "capabilities",
                "metadata",
                "ttl_seconds",
            ],
        )
        writer.writeheader()
        for i in range(n):
            writer.writerow(
                {
                    "id": str(i),
                    "timestamp_utc": "2026-06-12 12:06:02.895291-04",
                    "protocol_version": "1.0",
                    "agent": "claude",
                    "status": "online",
                    "session_id": "sess1",
                    "capabilities": "[]",
                    "metadata": "{}",
                    "ttl_seconds": "180",
                }
            )


def _run_main(monkeypatch, argv, post_responses):
    """Runs ipe.main() with sys.argv patched and ipe.post() replaced by a
    stub that returns the next canned response from `post_responses` (a
    list, consumed in call order) -- no network I/O happens."""
    calls = []

    def fake_post(url, token, payload, retries=5):
        assert token == "fake-token-for-tests"
        calls.append((url, payload))
        return post_responses.pop(0)

    monkeypatch.setattr(ipe, "post", fake_post)
    monkeypatch.setenv("AGENTBUS_TOKEN", "fake-token-for-tests")
    monkeypatch.setattr(sys, "argv", ["import_pg_export.py", *argv])
    exit_code = ipe.main()
    return exit_code, calls


def test_reads_and_counts_conflicts(tmp_path, monkeypatch):
    messages_csv = tmp_path / "messages.csv"
    _write_messages_csv(messages_csv, n=1)
    report = tmp_path / "report.json"

    exit_code, _ = _run_main(
        monkeypatch,
        [
            "--url",
            "https://example.invalid",
            "--origin-hub",
            "asuspro13",
            "--origin-host",
            "asuspro13.local",
            "--messages",
            str(messages_csv),
            "--report",
            str(report),
        ],
        post_responses=[
            {"accepted": [], "duplicates": [], "conflicts": ["m0"], "rejected": []}
        ],
    )
    assert exit_code == 0  # a conflict alone must never fail the run
    summary = json.loads(report.read_text())
    assert summary["messages"]["conflicts"] == 1
    assert summary["messages"]["conflict_ids"] == ["m0"]
    assert summary["messages"]["rejected"] == 0


def test_counts_presence_rejections(tmp_path, monkeypatch):
    presence_csv = tmp_path / "presence.csv"
    _write_presence_csv(presence_csv, n=1)
    report = tmp_path / "report.json"

    exit_code, _ = _run_main(
        monkeypatch,
        [
            "--url",
            "https://example.invalid",
            "--origin-hub",
            "asuspro13",
            "--origin-host",
            "asuspro13.local",
            "--presence",
            str(presence_csv),
            "--report",
            str(report),
        ],
        post_responses=[
            {
                "accepted": 0,
                "duplicates": 0,
                "rejected": [{"origin_id": 0, "reason": "status too long"}],
            }
        ],
    )
    assert exit_code == 1  # a presence rejection is a hard failure by default
    summary = json.loads(report.read_text())
    assert summary["presence"]["rejected"] == 1
    assert summary["presence"]["rejected_ids"] == [
        {"origin_id": 0, "reason": "status too long"}
    ]
    assert summary["total_rejected"] == 1


def test_message_rejection_is_a_hard_failure_by_default(tmp_path, monkeypatch):
    messages_csv = tmp_path / "messages.csv"
    _write_messages_csv(messages_csv, n=1)
    report = tmp_path / "report.json"

    exit_code, _ = _run_main(
        monkeypatch,
        [
            "--url",
            "https://example.invalid",
            "--origin-hub",
            "asuspro13",
            "--origin-host",
            "asuspro13.local",
            "--messages",
            str(messages_csv),
            "--report",
            str(report),
        ],
        post_responses=[
            {
                "accepted": [],
                "duplicates": [],
                "conflicts": [],
                "rejected": [{"id": "m0", "reason": "body too long"}],
            }
        ],
    )
    assert exit_code == 1
    summary = json.loads(report.read_text())
    assert summary["messages"]["rejected"] == 1
    assert summary["messages"]["rejected_ids"] == [
        {"id": "m0", "reason": "body too long"}
    ]


def test_allow_rejects_flag_makes_a_rejection_non_fatal(tmp_path, monkeypatch):
    messages_csv = tmp_path / "messages.csv"
    _write_messages_csv(messages_csv, n=1)
    report = tmp_path / "report.json"

    exit_code, _ = _run_main(
        monkeypatch,
        [
            "--url",
            "https://example.invalid",
            "--origin-hub",
            "asuspro13",
            "--origin-host",
            "asuspro13.local",
            "--messages",
            str(messages_csv),
            "--report",
            str(report),
            "--allow-rejects",
        ],
        post_responses=[
            {
                "accepted": [],
                "duplicates": [],
                "conflicts": [],
                "rejected": [{"id": "m0", "reason": "body too long"}],
            }
        ],
    )
    assert exit_code == 0
    # Still reported, even though it wasn't fatal.
    summary = json.loads(report.read_text())
    assert summary["messages"]["rejected"] == 1
    assert summary["allow_rejects"] is True


def test_token_is_never_read_from_argv_only_env(tmp_path, monkeypatch):
    """The bearer token must come from AGENTBUS_TOKEN only -- never argv,
    never the state file. This asserts the documented safety property."""
    messages_csv = tmp_path / "messages.csv"
    _write_messages_csv(messages_csv, n=1)
    report = tmp_path / "report.json"
    state = tmp_path / "state.json"

    monkeypatch.setenv("AGENTBUS_TOKEN", "fake-token-for-tests")
    monkeypatch.setattr(
        ipe,
        "post",
        lambda url, token, payload, retries=5: {
            "accepted": ["m0"],
            "duplicates": [],
            "conflicts": [],
            "rejected": [],
        },
    )
    monkeypatch.setattr(
        sys,
        "argv",
        [
            "import_pg_export.py",
            "--url",
            "https://example.invalid",
            "--origin-hub",
            "asuspro13",
            "--origin-host",
            "asuspro13.local",
            "--messages",
            str(messages_csv),
            "--report",
            str(report),
            "--state",
            str(state),
        ],
    )
    exit_code = ipe.main()
    assert exit_code == 0
    for text in (report.read_text(), state.read_text(), " ".join(sys.argv)):
        assert "fake-token-for-tests" not in text
