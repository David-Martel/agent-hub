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

import pytest

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


def _resume_args(tmp_path):
    messages = tmp_path / "messages.csv"
    presence = tmp_path / "presence.csv"
    _write_messages_csv(messages, n=2)
    _write_presence_csv(presence, n=2)
    return [
        "--url",
        "https://example.invalid",
        "--origin-hub",
        "asuspro13",
        "--origin-host",
        "asuspro13.local",
        "--messages",
        str(messages),
        "--presence",
        str(presence),
        "--state",
        str(tmp_path / "state.json"),
    ]


def _accepted():
    return [
        {"accepted": ["m0", "m1"], "duplicates": [], "conflicts": [], "rejected": []},
        {"accepted": 2, "duplicates": 0, "rejected": []},
    ]


def test_dry_run_then_real_import_does_not_skip_rows(tmp_path, monkeypatch):
    args = _resume_args(tmp_path)
    report = tmp_path / "report.json"
    code, calls = _run_main(
        monkeypatch, args + ["--dry-run", "--report", str(report)], []
    )
    assert code == 0 and calls == []
    assert not (tmp_path / "state.json").exists()
    assert not (tmp_path / "state.tmp").exists()
    assert not report.exists()
    code, calls = _run_main(monkeypatch, args, _accepted())
    assert code == 0
    assert [item["id"] for item in calls[0][1]["messages"]] == ["m0", "m1"]
    assert [item["origin_id"] for item in calls[1][1]["events"]] == [0, 1]


def test_dry_run_preserves_existing_state_and_report_bytes(
    tmp_path, monkeypatch, capsys
):
    args = _resume_args(tmp_path)
    _run_main(monkeypatch, args, _accepted())
    capsys.readouterr()
    state = tmp_path / "state.json"
    report = tmp_path / "report.json"
    report.write_bytes(b"existing report")
    before = state.read_bytes()
    code, calls = _run_main(
        monkeypatch, args + ["--dry-run", "--report", str(report)], []
    )
    assert code == 0 and calls == []
    assert state.read_bytes() == before
    assert report.read_bytes() == b"existing report"
    # A preview ignores completed offsets and examines all rows again.
    assert "messages 2/2" in capsys.readouterr().err


@pytest.mark.parametrize(
    "change",
    [
        "messages",
        "presence",
        "exclude_ids",
        "origin_hub",
        "origin_host",
        "url",
        "filters",
    ],
)
def test_resume_rejects_changed_inputs_before_any_post(tmp_path, monkeypatch, change):
    args = _resume_args(tmp_path)
    excluded = tmp_path / "excluded.txt"
    excluded.write_text("not-an-exported-id\n")
    args += ["--exclude-ids", str(excluded)]
    _run_main(monkeypatch, args, _accepted())
    state = tmp_path / "state.json"
    before = state.read_bytes()
    if change in ("messages", "presence", "exclude_ids"):
        path = Path(args[args.index("--" + change.replace("_", "-")) + 1])
        path.write_bytes(path.read_bytes() + b"\n")
    elif change == "filters":
        monkeypatch.setattr(ipe, "SECRET_PATTERNS", ipe.SECRET_PATTERNS[:-1])
    else:
        index = args.index("--" + change.replace("_", "-")) + 1
        args[index] = "changed"
    with pytest.raises(SystemExit, match="input or import settings changed"):
        _run_main(monkeypatch, args, [])
    assert state.read_bytes() == before


def test_same_bound_resume_sends_only_remaining_rows(tmp_path, monkeypatch):
    args = _resume_args(tmp_path)
    _run_main(monkeypatch, args, _accepted())
    path = tmp_path / "state.json"
    state = json.loads(path.read_text())
    state["messages_offset"] = 1
    state["presence_offset"] = 1
    path.write_text(json.dumps(state))
    code, calls = _run_main(monkeypatch, args, _accepted())
    assert code == 0
    assert [item["id"] for item in calls[0][1]["messages"]] == ["m1"]
    assert [item["origin_id"] for item in calls[1][1]["events"]] == [1]
    code, calls = _run_main(monkeypatch, args, [])
    assert code == 0 and calls == []


def test_interrupted_import_resumes_from_successful_batch(tmp_path, monkeypatch):
    args = _resume_args(tmp_path)
    monkeypatch.setattr(ipe, "BATCH", 1)
    monkeypatch.setenv("AGENTBUS_TOKEN", "fake-token-for-tests")
    monkeypatch.setattr(sys, "argv", ["import_pg_export.py", *args])
    calls = []

    def interrupted_post(url, token, payload, retries=5):
        calls.append(payload)
        if len(calls) == 2:
            raise SystemExit("synthetic transport failure")
        return {"accepted": ["m0"], "duplicates": [], "conflicts": [], "rejected": []}

    monkeypatch.setattr(ipe, "post", interrupted_post)
    with pytest.raises(SystemExit, match="synthetic transport failure"):
        ipe.main()
    state = json.loads((tmp_path / "state.json").read_text())
    assert state["messages_offset"] == 1 and state["binding"]["version"] == 1
    code, resumed = _run_main(
        monkeypatch,
        args,
        [
            {"accepted": ["m1"], "duplicates": [], "conflicts": [], "rejected": []},
            {"accepted": 1, "duplicates": 0, "rejected": []},
            {"accepted": 1, "duplicates": 0, "rejected": []},
        ],
    )
    assert code == 0
    assert [item["id"] for item in resumed[0][1]["messages"]] == ["m1"]
    assert [call[1]["events"][0]["origin_id"] for call in resumed[1:]] == [0, 1]


@pytest.mark.parametrize("offset", [1, -1, True, "1"])
def test_unbound_or_invalid_resume_offsets_refused(tmp_path, monkeypatch, offset):
    args = _resume_args(tmp_path)
    path = tmp_path / "state.json"
    path.write_text(json.dumps({"messages_offset": offset}))
    before = path.read_bytes()
    with pytest.raises(SystemExit, match="resume"):
        _run_main(monkeypatch, args, [])
    assert path.read_bytes() == before


def test_zero_legacy_offsets_can_establish_a_new_binding(tmp_path, monkeypatch):
    args = _resume_args(tmp_path)
    path = tmp_path / "state.json"
    path.write_text(json.dumps({"messages_offset": 0, "presence_offset": 0}))
    code, calls = _run_main(monkeypatch, args, _accepted())
    assert code == 0 and len(calls) == 2
    assert json.loads(path.read_text())["binding"]["version"] == 1
