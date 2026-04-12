//! Criterion throughput benchmarks for agent-bus token and encoding hotpaths.
//!
//! Since agent-bus is a binary crate (no `[lib]` target), all pure functions
//! are copied verbatim from the corresponding `src/` modules.  This accurately
//! measures algorithmic cost without needing a lib target.
//!
//! Functions benchmarked:
//! - `estimate_tokens` (from `token.rs`) — JSON vs natural-language text
//! - `minimize_value` (from `output.rs`) — short-key + default-stripping
//! - `encode_msgpack` / `decode_msgpack` (from `output.rs`) — `MessagePack` round-trip
//! - `format_message_toon` (from `output.rs`) — TOON encoding

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Types (mirror models.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Message {
    id: String,
    timestamp_utc: String,
    protocol_version: String,
    from: String,
    to: String,
    topic: String,
    body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    thread_id: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    priority: String,
    #[serde(default)]
    request_ack: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to: Option<String>,
    #[serde(default)]
    metadata: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Pure functions copied from src/token.rs
// ---------------------------------------------------------------------------

/// Estimate the number of LLM tokens in `text`.
///
/// Uses the same heuristic as `token.rs`: JSON-heavy text (>15% structural
/// characters) gets 2.5 chars/token, natural language gets 4.0 chars/token.
fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let total = text.chars().count();
    let json_chars = text
        .chars()
        .filter(|c| matches!(c, '{' | '}' | '[' | ']' | '"' | ':'))
        .count();
    let denominator: usize = if json_chars * 100 > total * 15 {
        25
    } else {
        40
    };
    (total * 10).div_ceil(denominator)
}

// ---------------------------------------------------------------------------
// Pure functions copied from src/output.rs
// ---------------------------------------------------------------------------

fn minimize_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                match k.as_str() {
                    "protocol_version" | "stream_id" => continue,
                    "tags" if v.as_array().is_some_and(Vec::is_empty) => continue,
                    "metadata" if v.as_object().is_some_and(serde_json::Map::is_empty) => {
                        continue;
                    }
                    "thread_id" if v.is_null() => continue,
                    "request_ack" if v == &serde_json::Value::Bool(false) => continue,
                    "priority" if v.as_str() == Some("normal") => continue,
                    _ => {}
                }
                let short = match k.as_str() {
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
                result.insert(short.to_owned(), minimize_value(v));
            }
            serde_json::Value::Object(result)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(minimize_value).collect())
        }
        other => other.clone(),
    }
}

fn encode_msgpack(value: &serde_json::Value) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec_named(value)
}

fn decode_msgpack(data: &[u8]) -> Result<serde_json::Value, rmp_serde::decode::Error> {
    rmp_serde::from_slice(data)
}

fn format_message_toon(msg: &Message) -> String {
    let cap = 10 + msg.from.len() + msg.to.len() + msg.topic.len() + 256;
    let mut out = String::with_capacity(cap);
    let _ = write!(out, "@{}→{} #{}", msg.from, msg.to, msg.topic);
    if !msg.tags.is_empty() {
        out.push_str(" [");
        let mut first = true;
        for tag in &msg.tags {
            if !first {
                out.push(',');
            }
            out.push_str(tag);
            first = false;
        }
        out.push(']');
    }
    out.push(' ');
    for (i, ch) in msg.body.chars().enumerate() {
        if i == 120 {
            break;
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------------------
// Test data factories
// ---------------------------------------------------------------------------

fn make_message(body_size: usize) -> Message {
    Message {
        id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
        timestamp_utc: "2026-03-20T10:30:00.123456Z".to_owned(),
        protocol_version: "1.0".to_owned(),
        from: "claude".to_owned(),
        to: "codex".to_owned(),
        topic: "rust-findings".to_owned(),
        body: "x".repeat(body_size),
        thread_id: Some("thread-abc-123".to_owned()),
        tags: vec!["repo:agent-hub".to_owned(), "severity:high".to_owned()],
        priority: "high".to_owned(),
        request_ack: true,
        reply_to: Some("claude".to_owned()),
        metadata: serde_json::json!({"_schema": "finding"}),
        stream_id: Some("1710504600123-0".to_owned()),
    }
}

fn make_json_value() -> serde_json::Value {
    serde_json::json!({
        "timestamp_utc": "2026-03-20T10:30:00.123456Z",
        "from": "claude",
        "to": "codex",
        "topic": "rust-findings",
        "body": "FINDING: excessive allocations in hot path\nSEVERITY: HIGH\nFix: use SmallVec",
        "tags": ["repo:agent-hub", "severity:high"],
        "priority": "high",
        "request_ack": true,
        "thread_id": "thread-abc-123",
        "metadata": {"_schema": "finding"},
        "protocol_version": "1.0",
        "stream_id": "1710504600123-0"
    })
}

// ---------------------------------------------------------------------------
// Benchmark 1: estimate_tokens
// ---------------------------------------------------------------------------

fn bench_estimate_tokens(c: &mut Criterion) {
    let mut group = c.benchmark_group("estimate_tokens");

    let natural_short = "Hello world, this is a short natural language sentence.";
    group.throughput(Throughput::Bytes(natural_short.len() as u64));
    group.bench_function("natural_short", |b| {
        b.iter(|| estimate_tokens(natural_short));
    });

    let natural_long = "The agent coordination bus provides real-time message passing \
        between AI coding agents running on the same machine. It uses Redis streams \
        for low-latency delivery and PostgreSQL for durable history. Agents register \
        their presence with a TTL so the orchestrator knows which agents are active.";
    group.throughput(Throughput::Bytes(natural_long.len() as u64));
    group.bench_function("natural_long", |b| {
        b.iter(|| estimate_tokens(natural_long));
    });

    // JSON-heavy text — triggers the denser 2.5 chars/token ratio.
    let json_text = r#"{"from":"claude","to":"codex","topic":"rust-findings","body":"FINDING: excessive alloc","tags":["repo:agent-hub","severity:high"],"priority":"high","request_ack":true}"#;
    group.throughput(Throughput::Bytes(json_text.len() as u64));
    group.bench_function("json_body", |b| {
        b.iter(|| estimate_tokens(json_text));
    });

    // Large NDJSON batch — representative of a bulk-read operation.
    let ndjson_batch = json_text.repeat(10);
    group.throughput(Throughput::Bytes(ndjson_batch.len() as u64));
    group.bench_function("ndjson_batch_10", |b| {
        b.iter(|| estimate_tokens(&ndjson_batch));
    });

    group.bench_function("empty_string", |b| {
        b.iter(|| estimate_tokens(""));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 2: minimize_value
// ---------------------------------------------------------------------------

fn bench_minimize_value(c: &mut Criterion) {
    let mut group = c.benchmark_group("minimize_value");

    // Message with all default fields — maximum stripping.
    let all_defaults = serde_json::json!({
        "timestamp_utc": "2026-03-20T10:30:00.123456Z",
        "from": "claude",
        "to": "all",
        "topic": "status",
        "body": "ready",
        "protocol_version": "1.0",
        "stream_id": "1234-0",
        "tags": [],
        "metadata": {},
        "thread_id": null,
        "request_ack": false,
        "priority": "normal"
    });
    group.bench_function("all_defaults_stripped", |b| {
        b.iter(|| minimize_value(&all_defaults));
    });

    // Message with non-default fields — no stripping, only renaming.
    let all_non_defaults = serde_json::json!({
        "timestamp_utc": "2026-03-20T10:30:00.123456Z",
        "from": "claude",
        "to": "codex",
        "topic": "rust-findings",
        "body": "FINDING: allocation in hot path\nSEVERITY: HIGH",
        "priority": "high",
        "request_ack": true,
        "tags": ["repo:agent-hub", "severity:high"],
        "metadata": {"_schema": "finding"},
        "thread_id": "thread-abc-123"
    });
    group.bench_function("all_non_defaults_renamed", |b| {
        b.iter(|| minimize_value(&all_non_defaults));
    });

    // Realistic mixed message (full protocol message).
    let realistic = make_json_value();
    group.bench_function("realistic_message", |b| {
        b.iter(|| minimize_value(&realistic));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 3: MessagePack encode / decode round-trip
// ---------------------------------------------------------------------------

fn bench_msgpack(c: &mut Criterion) {
    let mut group = c.benchmark_group("msgpack");

    // Small message (typical status update).
    let small = serde_json::json!({
        "f": "claude",
        "t": "codex",
        "tp": "status",
        "b": "ready",
        "ts": "2026-03-20T10:30:00Z"
    });
    let small_encoded = encode_msgpack(&small).expect("encode small");

    group.throughput(Throughput::Bytes(small_encoded.len() as u64));
    group.bench_function("encode_small", |b| {
        b.iter(|| encode_msgpack(&small).unwrap());
    });
    group.bench_function("decode_small", |b| {
        b.iter(|| decode_msgpack(&small_encoded).unwrap());
    });
    group.bench_function("round_trip_small", |b| {
        b.iter(|| {
            let encoded = encode_msgpack(&small).unwrap();
            decode_msgpack(&encoded).unwrap()
        });
    });

    // Full-size realistic message.
    let full = make_json_value();
    let full_encoded = encode_msgpack(&full).expect("encode full");

    group.throughput(Throughput::Bytes(full_encoded.len() as u64));
    group.bench_function("encode_full_message", |b| {
        b.iter(|| encode_msgpack(&full).unwrap());
    });
    group.bench_function("decode_full_message", |b| {
        b.iter(|| decode_msgpack(&full_encoded).unwrap());
    });
    group.bench_function("round_trip_full_message", |b| {
        b.iter(|| {
            let encoded = encode_msgpack(&full).unwrap();
            decode_msgpack(&encoded).unwrap()
        });
    });

    // Minimized message — smaller than full JSON, tests post-minimize encoding.
    let minimized = minimize_value(&full);
    let min_encoded = encode_msgpack(&minimized).expect("encode minimized");

    group.throughput(Throughput::Bytes(min_encoded.len() as u64));
    group.bench_function("round_trip_minimized", |b| {
        b.iter(|| {
            let encoded = encode_msgpack(&minimized).unwrap();
            decode_msgpack(&encoded).unwrap()
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 4: format_message_toon
// ---------------------------------------------------------------------------

fn bench_format_message_toon(c: &mut Criterion) {
    let mut group = c.benchmark_group("format_message_toon");

    let msg_short = make_message(50);
    group.bench_function("body_50_chars", |b| {
        b.iter(|| format_message_toon(&msg_short));
    });

    let msg_exact = make_message(120);
    group.bench_function("body_120_chars_exact_limit", |b| {
        b.iter(|| format_message_toon(&msg_exact));
    });

    let msg_long = make_message(500);
    group.bench_function("body_500_chars_truncated", |b| {
        b.iter(|| format_message_toon(&msg_long));
    });

    let msg_no_tags = {
        let mut m = make_message(80);
        m.tags.clear();
        m
    };
    group.bench_function("no_tags_body_80", |b| {
        b.iter(|| format_message_toon(&msg_no_tags));
    });

    // Batch of 10 messages — measures amortized cost.
    let batch: Vec<Message> = (0..10)
        .map(|i| {
            let mut m = make_message(80);
            m.from = format!("agent-{i}");
            m
        })
        .collect();
    group.bench_function("batch_10_messages", |b| {
        b.iter(|| batch.iter().map(format_message_toon).collect::<Vec<_>>());
    });

    // Varied body sizes to show throughput scaling.
    for size in [64usize, 256, 1024] {
        let msg = make_message(size);
        group.throughput(Throughput::Bytes(size as u64));
        group.bench_with_input(BenchmarkId::new("body_size", size), &msg, |b, msg| {
            b.iter(|| format_message_toon(msg));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion groups
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_estimate_tokens,
    bench_minimize_value,
    bench_msgpack,
    bench_format_message_toon,
);
criterion_main!(benches);
