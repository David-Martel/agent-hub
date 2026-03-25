//! Criterion throughput benchmarks for agent-bus-core token and encoding hotpaths.
//!
//! Functions benchmarked:
//! - `estimate_tokens` (from `token`) — JSON vs natural-language text
//! - `minimize_value` (from `output`) — short-key + default-stripping
//! - `encode_msgpack` / `decode_msgpack` (from `output`) — `MessagePack` round-trip
//! - `format_message_toon` (from `output`) — TOON encoding

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use agent_bus_core::models::Message;
use agent_bus_core::output::{
    decode_msgpack, encode_msgpack, format_message_toon, minimize_value,
};
use agent_bus_core::token::estimate_tokens;

// ---------------------------------------------------------------------------
// Test data factories
// ---------------------------------------------------------------------------

fn make_message(body_size: usize) -> Message {
    let json = serde_json::json!({
        "id": "550e8400-e29b-41d4-a716-446655440000",
        "timestamp_utc": "2026-03-20T10:30:00.123456Z",
        "protocol_version": "1.0",
        "from": "claude",
        "to": "codex",
        "topic": "rust-findings",
        "body": "x".repeat(body_size),
        "thread_id": "thread-abc-123",
        "tags": ["repo:agent-hub", "severity:high"],
        "priority": "high",
        "request_ack": true,
        "reply_to": "claude",
        "metadata": {"_schema": "finding"},
        "stream_id": "1710504600123-0",
    });
    serde_json::from_value(json).expect("make_message")
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

    let json_text = r#"{"from":"claude","to":"codex","topic":"rust-findings","body":"FINDING: excessive alloc","tags":["repo:agent-hub","severity:high"],"priority":"high","request_ack":true}"#;
    group.throughput(Throughput::Bytes(json_text.len() as u64));
    group.bench_function("json_body", |b| {
        b.iter(|| estimate_tokens(json_text));
    });

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

    let msg_no_tags: Message = {
        let mut json = serde_json::to_value(make_message(80)).unwrap();
        json["tags"] = serde_json::json!([]);
        serde_json::from_value(json).unwrap()
    };
    group.bench_function("no_tags_body_80", |b| {
        b.iter(|| format_message_toon(&msg_no_tags));
    });

    let batch: Vec<Message> = (0..10)
        .map(|i| {
            let mut json = serde_json::to_value(make_message(80)).unwrap();
            json["from"] = serde_json::json!(format!("agent-{i}"));
            serde_json::from_value(json).unwrap()
        })
        .collect();
    group.bench_function("batch_10_messages", |b| {
        b.iter(|| batch.iter().map(format_message_toon).collect::<Vec<_>>());
    });

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
