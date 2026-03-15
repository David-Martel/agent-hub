from __future__ import annotations

import json as _json
from typing import Any


def _load_backend() -> tuple[str, str, bool]:
    """Load a native codec backend if available, otherwise use stdlib JSON."""
    try:
        from . import _codec_native

        backend_name = _codec_native.get_backend_name()
        is_native = backend_name != "python"
        backend_id = "pyo3" if is_native else "python"
        return backend_id, backend_name, is_native
    except Exception:
        return "json", "stdlib", False


_BACKEND_ID, _BACKEND_NAME, _HAS_NATIVE_CODEC = _load_backend()


# ---------------------------------------------------------------------------
# Serialization (Python → JSON string)
# ---------------------------------------------------------------------------


def dumps_compact(data: Any) -> str:
    """Serialize to compact JSON (no whitespace)."""
    if _BACKEND_ID == "pyo3":
        from . import _codec_native

        return _codec_native.dumps_compact(data)
    return _json.dumps(data, separators=(",", ":"))


def dumps_pretty(data: Any) -> str:
    """Serialize to pretty-printed JSON (2-space indent)."""
    if _BACKEND_ID == "pyo3":
        from . import _codec_native

        return _codec_native.dumps_pretty(data)
    return _json.dumps(data, indent=2)


def serialize_stream_payload(payload: dict[str, Any]) -> dict[str, str]:
    """Convert a message payload dict to Redis stream format.

    Complex values (list, dict, bool) become compact JSON strings.
    Simple values become str(). None values are dropped.
    """
    if _BACKEND_ID == "pyo3":
        from . import _codec_native

        return _codec_native.serialize_stream_payload(payload)

    return {
        key: dumps_compact(value) if isinstance(value, (list, dict, bool)) else str(value)
        for key, value in payload.items()
        if value is not None
    }


# ---------------------------------------------------------------------------
# Deserialization (JSON string → Python)
# ---------------------------------------------------------------------------


def parse_compact(json_str: str) -> Any:
    """Parse a compact JSON string into a Python object.

    Uses Rust serde_json when available, falls back to stdlib json.loads.
    """
    if _BACKEND_ID == "pyo3":
        from . import _codec_native

        return _codec_native.parse_compact(json_str)
    return _json.loads(json_str)


def decode_stream_entry(entry: dict[str, str]) -> dict[str, Any]:
    """Decode a Redis stream entry, parsing JSON for complex fields.

    Converts tags/metadata from JSON strings, request_ack from string to bool.
    """
    if _BACKEND_ID == "pyo3":
        from . import _codec_native

        return _codec_native.decode_stream_entry(entry)

    result: dict[str, Any] = {}
    for key, value in entry.items():
        if key in ("tags", "metadata"):
            result[key] = _json.loads(value)
        elif key == "request_ack":
            result[key] = value in ("true", "True")
        else:
            result[key] = value
    return result


def decode_stream_entries(entries: list[dict[str, str]]) -> list[dict[str, Any]]:
    """Batch-decode multiple Redis stream entries."""
    if _BACKEND_ID == "pyo3":
        from . import _codec_native

        return _codec_native.decode_stream_entries(entries)
    return [decode_stream_entry(e) for e in entries]


# ---------------------------------------------------------------------------
# Token estimation and context compression
# ---------------------------------------------------------------------------


def estimate_tokens(text: str) -> int:
    """Estimate token count using a fast heuristic (~4 chars/token)."""
    if _BACKEND_ID == "pyo3":
        from . import _codec_native

        return _codec_native.estimate_tokens(text)

    n = len(text)
    if n == 0:
        return 0
    json_chars = sum(1 for c in text if c in "{}[]\":,")
    json_ratio = json_chars / n
    chars_per_token = 4.0 - (json_ratio * 1.5)
    return int(n / chars_per_token + 0.99)


def minimize_message(message: dict[str, Any]) -> dict[str, Any]:
    """Produce a token-minimized message dict.

    Strips defaults/empties, shortens field names for inter-agent context sharing.
    """
    if _BACKEND_ID == "pyo3":
        from . import _codec_native

        return _codec_native.minimize_message(message)

    field_map = {
        "timestamp_utc": "ts", "request_ack": "ack", "from": "f", "to": "t",
        "topic": "tp", "body": "b", "priority": "p", "reply_to": "rt",
        "thread_id": "tid", "tags": "tg", "metadata": "m",
    }
    skip_fields = {"protocol_version", "stream_id"}
    result: dict[str, Any] = {}
    for k, v in message.items():
        if k in skip_fields:
            continue
        if k == "tags" and isinstance(v, list) and len(v) == 0:
            continue
        if k == "metadata" and isinstance(v, dict) and len(v) == 0:
            continue
        if k == "thread_id" and v is None:
            continue
        if k == "request_ack" and v is False:
            continue
        if k == "priority" and v == "normal":
            continue
        result[field_map.get(k, k)] = v
    return result


def compact_context(messages: list[dict[str, Any]], max_tokens: int) -> list[dict[str, Any]]:
    """Compact a list of messages to fit within a token budget.

    Returns the most recent messages (minimized) that fit within max_tokens.
    """
    if _BACKEND_ID == "pyo3":
        from . import _codec_native

        return _codec_native.compact_context(messages, max_tokens)

    result: list[dict[str, Any]] = []
    budget = max_tokens
    for msg in reversed(messages):
        minimized = minimize_message(msg)
        tokens = estimate_tokens(dumps_compact(minimized))
        if tokens > budget:
            break
        budget -= tokens
        result.insert(0, minimized)
    return result


# ---------------------------------------------------------------------------
# Metadata
# ---------------------------------------------------------------------------


def codec_metadata() -> dict[str, Any]:
    return {
        "codec": _BACKEND_ID,
        "backend": _BACKEND_NAME,
        "native": _HAS_NATIVE_CODEC,
    }
