//! Native codec extension for agent-bus-mcp.
//!
//! Provides high-performance JSON serialization, deserialization,
//! token estimation, and context compression for the agent bus
//! coordination protocol.

mod codec;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

// ---------------------------------------------------------------------------
// Backend identification
// ---------------------------------------------------------------------------

/// Returns the backend name for codec metadata reporting.
#[pyfunction]
fn get_backend_name() -> &'static str {
    "rust-serde"
}

// ---------------------------------------------------------------------------
// Serialization (Python → JSON string)
// ---------------------------------------------------------------------------

/// Serialize data to compact JSON (no whitespace).
#[pyfunction]
fn dumps_compact(py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<String> {
    codec::dumps_compact(py, data)
}

/// Serialize data to pretty-printed JSON (2-space indent).
#[pyfunction]
fn dumps_pretty(py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<String> {
    codec::dumps_pretty(py, data)
}

/// Batch-serialize a message payload dict to Redis stream format.
#[pyfunction]
fn serialize_stream_payload(
    py: Python<'_>,
    payload: &Bound<'_, PyDict>,
) -> PyResult<std::collections::HashMap<String, String>> {
    codec::serialize_stream_payload(py, payload)
}

// ---------------------------------------------------------------------------
// Deserialization (JSON string → Python)
// ---------------------------------------------------------------------------

/// Parse a compact JSON string into a Python object.
///
/// Replaces `json.loads()` on hot paths (watch loop, stream entry decoding).
#[pyfunction]
fn parse_compact(py: Python<'_>, json_str: &str) -> PyResult<PyObject> {
    codec::parse_compact(py, json_str)
}

/// Decode a single Redis stream entry, parsing JSON fields.
///
/// Takes `dict[str, str]` and returns `dict[str, Any]` with tags/metadata parsed.
#[pyfunction]
fn decode_stream_entry(py: Python<'_>, entry: &Bound<'_, PyDict>) -> PyResult<PyObject> {
    codec::decode_stream_entry(py, entry)
}

/// Batch-decode multiple Redis stream entries.
#[pyfunction]
fn decode_stream_entries(py: Python<'_>, entries: &Bound<'_, PyList>) -> PyResult<PyObject> {
    codec::decode_stream_entries(py, entries)
}

// ---------------------------------------------------------------------------
// Token estimation and context compression
// ---------------------------------------------------------------------------

/// Estimate token count for a string using a fast heuristic.
///
/// ~4 chars/token for text, ~2.5 for JSON-heavy content. Fast, not exact.
#[pyfunction]
fn estimate_tokens(text: &str) -> usize {
    codec::estimate_tokens(text)
}

/// Produce a token-minimized message dict.
///
/// Strips defaults/empties, shortens field names (`timestamp_utc`→`ts`, `from`→`f`, etc).
#[pyfunction]
fn minimize_message(py: Python<'_>, message: &Bound<'_, PyDict>) -> PyResult<PyObject> {
    codec::minimize_message(py, message)
}

/// Compact a list of messages to fit within a token budget.
///
/// Returns the most recent messages (minimized) that fit within `max_tokens`.
#[pyfunction]
fn compact_context(
    py: Python<'_>,
    messages: &Bound<'_, PyList>,
    max_tokens: usize,
) -> PyResult<PyObject> {
    codec::compact_context(py, messages, max_tokens)
}

// ---------------------------------------------------------------------------
// MessagePack serialization
// ---------------------------------------------------------------------------

/// Serialize data to `MessagePack` binary format (returns bytes).
///
/// ~30-50% smaller than JSON for typical agent bus messages.
#[pyfunction]
fn dumps_msgpack<'py>(py: Python<'py>, data: &Bound<'_, PyAny>) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
    let value = codec::py_to_value(py, data)?;
    let bytes = rmp_serde::to_vec(&value)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    Ok(pyo3::types::PyBytes::new(py, &bytes))
}

/// Deserialize `MessagePack` binary data into a Python object.
#[pyfunction]
fn loads_msgpack(py: Python<'_>, data: &[u8]) -> PyResult<PyObject> {
    let value: serde_json::Value = rmp_serde::from_slice(data)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    codec::value_to_py(py, &value)
}

// ---------------------------------------------------------------------------
// LZ4 compression
// ---------------------------------------------------------------------------

/// Compress bytes with LZ4 (frame format). Returns compressed bytes.
#[pyfunction]
fn lz4_compress<'py>(py: Python<'py>, data: &[u8]) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
    use std::io::Write as _;
    let mut encoder = lz4_flex::frame::FrameEncoder::new(Vec::new());
    encoder
        .write_all(data)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    let result = encoder
        .finish()
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    Ok(pyo3::types::PyBytes::new(py, &result))
}

/// Decompress LZ4-compressed bytes. Returns decompressed bytes.
#[pyfunction]
fn lz4_decompress<'py>(py: Python<'py>, data: &[u8]) -> PyResult<Bound<'py, pyo3::types::PyBytes>> {
    use std::io::Read as _;
    let mut decoder = lz4_flex::frame::FrameDecoder::new(data);
    let mut result = Vec::new();
    decoder.read_to_end(&mut result)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
    Ok(pyo3::types::PyBytes::new(py, &result))
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

/// Native codec module for agent-bus-mcp.
#[pymodule]
fn _codec_native(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Serialization
    m.add_function(wrap_pyfunction!(get_backend_name, m)?)?;
    m.add_function(wrap_pyfunction!(dumps_compact, m)?)?;
    m.add_function(wrap_pyfunction!(dumps_pretty, m)?)?;
    m.add_function(wrap_pyfunction!(serialize_stream_payload, m)?)?;
    // Deserialization
    m.add_function(wrap_pyfunction!(parse_compact, m)?)?;
    m.add_function(wrap_pyfunction!(decode_stream_entry, m)?)?;
    m.add_function(wrap_pyfunction!(decode_stream_entries, m)?)?;
    // Token/context
    m.add_function(wrap_pyfunction!(estimate_tokens, m)?)?;
    m.add_function(wrap_pyfunction!(minimize_message, m)?)?;
    m.add_function(wrap_pyfunction!(compact_context, m)?)?;
    // MessagePack
    m.add_function(wrap_pyfunction!(dumps_msgpack, m)?)?;
    m.add_function(wrap_pyfunction!(loads_msgpack, m)?)?;
    // LZ4
    m.add_function(wrap_pyfunction!(lz4_compress, m)?)?;
    m.add_function(wrap_pyfunction!(lz4_decompress, m)?)?;
    Ok(())
}
