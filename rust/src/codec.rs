//! JSON serialization using `serde_json`.
//!
//! Converts between Python objects and JSON strings via `serde_json::Value`.
//! Handles all JSON-compatible Python types: None, bool, int, float, str, list, dict.

use std::collections::HashMap;

use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyNone, PyString};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Python ↔ serde_json conversion
// ---------------------------------------------------------------------------

/// Convert a Python object to a `serde_json::Value`.
///
/// Public so `lib.rs` can use it for `MessagePack` serialization.
#[expect(clippy::only_used_in_recursion, reason = "py token required by PyO3 for GIL safety")]
pub fn py_to_value(py: Python<'_>, obj: &Bound<'_, PyAny>) -> PyResult<Value> {
    if obj.is_instance_of::<PyNone>() {
        Ok(Value::Null)
    } else if let Ok(b) = obj.downcast::<PyBool>() {
        Ok(Value::Bool(b.is_true()))
    } else if let Ok(i) = obj.downcast::<PyInt>() {
        let v: i64 = i.extract()?;
        Ok(Value::Number(v.into()))
    } else if let Ok(f) = obj.downcast::<PyFloat>() {
        let v: f64 = f.extract()?;
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| {
                pyo3::exceptions::PyValueError::new_err("NaN/Inf not supported in JSON")
            })
    } else if let Ok(s) = obj.downcast::<PyString>() {
        Ok(Value::String(s.to_str()?.to_owned()))
    } else if let Ok(list) = obj.downcast::<PyList>() {
        let items: Result<Vec<Value>, _> =
            list.iter().map(|item| py_to_value(py, &item)).collect();
        Ok(Value::Array(items?))
    } else if let Ok(dict) = obj.downcast::<PyDict>() {
        let mut map = serde_json::Map::new();
        for (key, val) in dict.iter() {
            let k: String = key.extract()?;
            map.insert(k, py_to_value(py, &val)?);
        }
        Ok(Value::Object(map))
    } else {
        let s: String = obj.str()?.extract()?;
        Ok(Value::String(s))
    }
}

/// Convert a `serde_json::Value` to a Python object.
///
/// Public so `lib.rs` can use it for `MessagePack` deserialization.
pub fn value_to_py(py: Python<'_>, value: &Value) -> PyResult<PyObject> {
    match value {
        Value::Null => Ok(py.None()),
        Value::Bool(b) => Ok(PyBool::new(py, *b).to_owned().into_any().unbind()),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into_pyobject(py)?.into_any().unbind())
            } else if let Some(f) = n.as_f64() {
                Ok(f.into_pyobject(py)?.into_any().unbind())
            } else {
                Ok(py.None())
            }
        }
        Value::String(s) => Ok(s.into_pyobject(py)?.into_any().unbind()),
        Value::Array(arr) => {
            let list = PyList::empty(py);
            for item in arr {
                list.append(value_to_py(py, item)?)?;
            }
            Ok(list.into_any().unbind())
        }
        Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, value_to_py(py, v)?)?;
            }
            Ok(dict.into_any().unbind())
        }
    }
}

// ---------------------------------------------------------------------------
// Serialization (Python → JSON string)
// ---------------------------------------------------------------------------

/// Serialize to compact JSON (no whitespace).
pub fn dumps_compact(py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<String> {
    let value = py_to_value(py, data)?;
    serde_json::to_string(&value)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
}

/// Serialize to pretty JSON (2-space indent).
pub fn dumps_pretty(py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<String> {
    let value = py_to_value(py, data)?;
    serde_json::to_string_pretty(&value)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))
}

/// Convert a Python dict to a Redis stream payload dict.
///
/// Complex values (list, dict, bool) become compact JSON strings.
/// Simple values become `str()`. `None` values are dropped.
pub fn serialize_stream_payload(
    py: Python<'_>,
    payload: &Bound<'_, PyDict>,
) -> PyResult<HashMap<String, String>> {
    let mut result = HashMap::new();
    for (key, value) in payload.iter() {
        if value.is_instance_of::<PyNone>() {
            continue;
        }
        let k: String = key.extract()?;
        let v = if value.is_instance_of::<PyDict>()
            || value.is_instance_of::<PyList>()
            || value.is_instance_of::<PyBool>()
        {
            dumps_compact(py, &value)?
        } else {
            value.str()?.to_str()?.to_owned()
        };
        result.insert(k, v);
    }
    Ok(result)
}

// ---------------------------------------------------------------------------
// Deserialization (JSON string → Python)
// ---------------------------------------------------------------------------

/// Parse a compact JSON string into a Python object.
///
/// Returns dict, list, str, int, float, bool, or None depending on the JSON.
/// This replaces `json.loads()` on hot paths (watch loop, stream decoding).
pub fn parse_compact(py: Python<'_>, json_str: &str) -> PyResult<PyObject> {
    let value: Value = serde_json::from_str(json_str)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    value_to_py(py, &value)
}

/// Decode a Redis stream entry's fields, parsing JSON for complex fields.
///
/// Takes a `dict[str, str]` (raw Redis stream entry) and returns a
/// `dict[str, Any]` with `tags`, `metadata`, `request_ack` parsed from JSON.
/// Bool-like strings ("true"/"false") for `request_ack` are converted to bool.
pub fn decode_stream_entry(
    py: Python<'_>,
    entry: &Bound<'_, PyDict>,
) -> PyResult<PyObject> {
    let result = PyDict::new(py);
    for (key, value) in entry.iter() {
        let k: String = key.extract()?;
        let v: String = value.extract()?;

        let parsed: PyObject = match k.as_str() {
            "tags" | "metadata" => parse_compact(py, &v)?,
            "request_ack" => {
                let b = v == "true" || v == "True";
                PyBool::new(py, b).to_owned().into_any().unbind()
            }
            _ => v.into_pyobject(py)?.into_any().unbind(),
        };
        result.set_item(k, parsed)?;
    }
    Ok(result.into_any().unbind())
}

/// Batch-decode multiple Redis stream entries.
///
/// Takes a list of `dict[str, str]` entries and returns a list of decoded dicts.
pub fn decode_stream_entries(
    py: Python<'_>,
    entries: &Bound<'_, PyList>,
) -> PyResult<PyObject> {
    let result = PyList::empty(py);
    for entry in entries.iter() {
        let dict = entry.downcast::<PyDict>().map_err(|e| {
            pyo3::exceptions::PyTypeError::new_err(format!("entries must be list of dicts: {e}"))
        })?;
        result.append(decode_stream_entry(py, dict)?)?;
    }
    Ok(result.into_any().unbind())
}

// ---------------------------------------------------------------------------
// Token estimation and context compression
// ---------------------------------------------------------------------------

/// Estimate token count for a string using a fast heuristic.
///
/// Uses the approximation: 1 token ~= 4 characters for English text,
/// adjusted for JSON overhead (braces, quotes, colons add ~1.3x).
/// This is intentionally fast (no tokenizer), not exact.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "Token estimation is approximate; precision loss is acceptable"
)]
pub fn estimate_tokens(text: &str) -> usize {
    let char_count = text.len();
    if char_count == 0 {
        return 0;
    }
    // JSON-heavy text: ~3.2 chars/token; natural text: ~4 chars/token
    let json_chars = text
        .bytes()
        .filter(|b| matches!(b, b'{' | b'}' | b'[' | b']' | b'"' | b':' | b','))
        .count();
    let json_ratio = json_chars as f64 / char_count as f64;
    let chars_per_token = 4.0 - (json_ratio * 1.5); // 4.0 for text, 2.5 for pure JSON
    (char_count as f64 / chars_per_token).ceil() as usize
}

/// Produce a token-minimized representation of a message.
///
/// Strips `protocol_version`, `stream_id`, `metadata` (if empty),
/// `tags` (if empty), `thread_id` (if null), `reply_to` (if same as sender).
/// Shortens field names: `timestamp_utc` → `ts`, `request_ack` → `ack`.
pub fn minimize_message(py: Python<'_>, message: &Bound<'_, PyDict>) -> PyResult<PyObject> {
    let result = PyDict::new(py);

    // Field mapping: original → minimized (None = skip)
    for (key, value) in message.iter() {
        let k: String = key.extract()?;

        // Skip empty/default fields
        match k.as_str() {
            "protocol_version" | "stream_id" => continue,
            "tags" => {
                if let Ok(list) = value.downcast::<PyList>() {
                    if list.len() == 0 {
                        continue;
                    }
                }
            }
            "metadata" => {
                if let Ok(dict) = value.downcast::<PyDict>() {
                    if dict.len() == 0 {
                        continue;
                    }
                }
            }
            "thread_id" => {
                if value.is_instance_of::<PyNone>() {
                    continue;
                }
            }
            "request_ack" => {
                if let Ok(b) = value.downcast::<PyBool>() {
                    if !b.is_true() {
                        continue; // Skip false (default)
                    }
                }
            }
            "priority" => {
                if let Ok(s) = value.downcast::<PyString>() {
                    if s.to_str()? == "normal" {
                        continue; // Skip default priority
                    }
                }
            }
            _ => {}
        }

        // Shorten field names
        let short_key = match k.as_str() {
            "timestamp_utc" => "ts",
            "request_ack" => "ack",
            "from" => "f",
            "to" => "t",
            "topic" => "tp",
            "body" => "b",
            "priority" => "p",
            "reply_to" => "rt",
            "thread_id" => "tid",
            "tags" => "tg",
            "metadata" => "m",
            other => other,
        };

        result.set_item(short_key, value)?;
    }

    Ok(result.into_any().unbind())
}

/// Compact a list of messages to fit within a token budget.
///
/// Returns the most recent messages that fit within `max_tokens`.
/// Messages are minimized first, then serialized to estimate token cost.
pub fn compact_context(
    py: Python<'_>,
    messages: &Bound<'_, PyList>,
    max_tokens: usize,
) -> PyResult<PyObject> {
    let result = PyList::empty(py);
    let mut budget = max_tokens;

    // Process from newest to oldest (last element first)
    let len = messages.len();
    for i in (0..len).rev() {
        let msg = messages.get_item(i)?;
        let dict = msg.downcast::<PyDict>().map_err(|e| {
            pyo3::exceptions::PyTypeError::new_err(format!("messages must be list of dicts: {e}"))
        })?;
        let minimized = minimize_message(py, dict)?;
        let minimized_ref = minimized.bind(py);
        let json = dumps_compact(py, minimized_ref)?;
        let tokens = estimate_tokens(&json);
        if tokens > budget {
            break;
        }
        budget -= tokens;
        result.insert(0, minimized_ref)?;
    }

    Ok(result.into_any().unbind())
}
