"""Tests for codec backends (Python fallback and Rust/PyO3 native).

Both backends must produce identical JSON output for all supported types.
"""

import json

import pytest

from agent_bus_mcp.codec import codec_metadata, dumps_compact, dumps_pretty


# --- Fixtures ---

ROUND_TRIP_CASES = [
    ("null", None),
    ("bool_true", True),
    ("bool_false", False),
    ("int_zero", 0),
    ("int_positive", 42),
    ("int_negative", -1),
    ("float", 3.14),
    ("empty_string", ""),
    ("ascii_string", "hello"),
    ("unicode_string", "caf\u00e9 \U0001f600"),
    ("empty_list", []),
    ("nested_list", [[1, 2], [3, [4, 5]]]),
    ("empty_dict", {}),
    ("flat_dict", {"a": 1, "b": "two", "c": True}),
    ("nested_dict", {"outer": {"inner": [1, 2, 3]}}),
    (
        "message_payload",
        {
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "timestamp_utc": "2026-03-14T12:00:00Z",
            "protocol_version": "1.0",
            "from": "claude",
            "to": "codex",
            "topic": "review-findings",
            "body": "Found 3 issues in src/main.rs",
            "tags": ["review", "rust-pro"],
            "priority": "high",
            "request_ack": True,
            "reply_to": "claude",
            "metadata": {"agent_type": "rust-pro", "file_count": 12},
        },
    ),
]


class TestCodecMetadata:
    def test_metadata_has_required_keys(self):
        meta = codec_metadata()
        assert "codec" in meta
        assert "backend" in meta
        assert "native" in meta
        assert isinstance(meta["native"], bool)

    def test_backend_is_string(self):
        meta = codec_metadata()
        assert isinstance(meta["backend"], str)
        assert len(meta["backend"]) > 0


class TestRoundTrip:
    @pytest.mark.parametrize(
        "name,data", ROUND_TRIP_CASES, ids=[c[0] for c in ROUND_TRIP_CASES]
    )
    def test_compact_round_trip(self, name, data):
        result = dumps_compact(data)
        parsed = json.loads(result)
        assert parsed == data

    @pytest.mark.parametrize(
        "name,data", ROUND_TRIP_CASES, ids=[c[0] for c in ROUND_TRIP_CASES]
    )
    def test_pretty_round_trip(self, name, data):
        result = dumps_pretty(data)
        parsed = json.loads(result)
        assert parsed == data

    @pytest.mark.parametrize(
        "name,data", ROUND_TRIP_CASES, ids=[c[0] for c in ROUND_TRIP_CASES]
    )
    def test_compact_semantically_matches_stdlib(self, name, data):
        """Native codec must produce semantically identical output to stdlib json.

        Note: serde_json may differ from stdlib json in key ordering (sorted vs
        insertion-order) and unicode escaping (UTF-8 vs \\uXXXX). Both are valid
        JSON. We compare parsed values, not raw strings.
        """
        native = dumps_compact(data)
        reference = json.dumps(data, separators=(",", ":"))
        assert json.loads(native) == json.loads(reference)


class TestEdgeCases:
    def test_large_message_batch(self):
        """Simulate batch of 100 messages."""
        messages = [
            {"id": f"msg-{i}", "body": f"message {i}", "tags": [f"tag-{i % 5}"]}
            for i in range(100)
        ]
        for msg in messages:
            result = dumps_compact(msg)
            assert json.loads(result) == msg

    def test_deeply_nested(self):
        data: dict = {"level": 0}
        current = data
        for i in range(1, 20):
            current["child"] = {"level": i}
            current = current["child"]
        result = dumps_compact(data)
        assert json.loads(result) == data
