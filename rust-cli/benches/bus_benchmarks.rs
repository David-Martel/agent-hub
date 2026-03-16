//! Criterion benchmarks for agent-bus hotpath functions.
//!
//! Since agent-bus is a binary crate, these benchmarks re-implement the pure
//! computation paths using the same dependencies.  This accurately measures
//! the algorithmic cost without needing a lib target.

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Presence {
    agent: String,
    status: String,
    protocol_version: String,
    timestamp_utc: String,
    session_id: String,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default)]
    metadata: serde_json::Value,
    ttl_seconds: u64,
}

#[derive(Debug, Clone, Serialize)]
struct Health {
    ok: bool,
    protocol_version: String,
    redis_url: String,
    database_url: Option<String>,
    database_ok: Option<bool>,
    database_error: Option<String>,
    storage_ready: bool,
    runtime: String,
    codec: String,
    stream_length: Option<u64>,
    pg_message_count: Option<i64>,
    pg_presence_count: Option<i64>,
}

// ---------------------------------------------------------------------------
// Pure functions (mirror output.rs — keep in sync with src/output.rs)
// ---------------------------------------------------------------------------

use std::fmt::Write as _;

fn format_message_toon(msg: &Message) -> String {
    // Optimized: single pre-sized String, no intermediate join/format allocations.
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

fn format_presence_toon(p: &Presence) -> String {
    // Optimized: single pre-sized String, no join/format intermediate allocations.
    let cap = 2 + p.agent.len() + 1 + p.status.len() + 64;
    let mut out = String::with_capacity(cap);
    let _ = write!(out, "~{} {}", p.agent, p.status);
    if !p.capabilities.is_empty() {
        out.push_str(" [");
        let mut first = true;
        for c in &p.capabilities {
            if !first {
                out.push(',');
            }
            out.push_str(c);
            first = false;
        }
        out.push(']');
    }
    let _ = write!(out, " ttl={}s", p.ttl_seconds);
    out
}

fn format_health_toon(h: &Health) -> String {
    // Optimized: pre-sized String, write! directly — avoids two intermediate String allocs.
    let mut out = String::with_capacity(72);
    let _ = write!(out, "ok={} r=", h.ok);
    match h.stream_length {
        Some(n) => {
            let _ = write!(out, "{n}");
        }
        None => out.push('?'),
    }
    out.push_str(" p=");
    match h.pg_message_count {
        Some(n) => {
            let _ = write!(out, "{n}");
        }
        None => out.push('?'),
    }
    let _ = write!(out, " v={}", h.protocol_version);
    out
}

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

// ---------------------------------------------------------------------------
// Pure functions (mirror validation.rs)
// ---------------------------------------------------------------------------

fn infer_schema_from_topic<'a>(topic: &str, explicit_schema: Option<&'a str>) -> Option<&'a str> {
    if explicit_schema.is_some() {
        return explicit_schema;
    }
    match topic {
        t if t.contains("findings") || t.starts_with("review") => Some("finding"),
        "status" | "ownership" | "coordination" | "handoff" => Some("status"),
        "benchmark" => Some("benchmark"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Pure functions (mirror channels.rs key helpers)
// ---------------------------------------------------------------------------

const DIRECT_PREFIX: &str = "bus:direct:";
const GROUP_PREFIX: &str = "bus:group:";
const CLAIMS_PREFIX: &str = "bus:claims:";
const MEMBERS_SUFFIX: &str = ":members";

fn direct_key(agent_a: &str, agent_b: &str) -> String {
    let (lo, hi) = if agent_a <= agent_b {
        (agent_a, agent_b)
    } else {
        (agent_b, agent_a)
    };
    format!("{DIRECT_PREFIX}{lo}:{hi}")
}

fn group_stream_key(name: &str) -> String {
    format!("{GROUP_PREFIX}{name}")
}

fn group_members_key(name: &str) -> String {
    format!("{GROUP_PREFIX}{name}{MEMBERS_SUFFIX}")
}

fn claims_key(resource: &str) -> String {
    let normalised = resource.replace('\\', "/");
    format!("{CLAIMS_PREFIX}{normalised}")
}

// ---------------------------------------------------------------------------
// Pure functions (mirror redis_bus.rs decode_stream_entry — key extraction)
// ---------------------------------------------------------------------------

/// Original approach: `from_utf8_lossy` → Cow → `.to_string()` (two potential allocs).
fn get_field_original(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw).to_string()
}

/// Optimized approach: try `from_utf8` first (zero-copy path), fall back to lossy.
fn get_field_optimized(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_owned(),
        Err(_) => String::from_utf8_lossy(raw).into_owned(),
    }
}

/// Simulated decode: extracts 8 fields from a `HashMap` of byte slices.
fn decode_fields_original(fields: &std::collections::HashMap<&str, Vec<u8>>) -> Vec<String> {
    let get = |k: &str| -> String {
        fields
            .get(k)
            .map_or_else(String::new, |b| get_field_original(b))
    };
    vec![
        get("id"),
        get("timestamp_utc"),
        get("from"),
        get("to"),
        get("topic"),
        get("body"),
        get("priority"),
        get("tags"),
    ]
}

fn decode_fields_optimized(fields: &std::collections::HashMap<&str, Vec<u8>>) -> Vec<String> {
    let get = |k: &str| -> String {
        fields
            .get(k)
            .map_or_else(String::new, |b| get_field_optimized(b))
    };
    vec![
        get("id"),
        get("timestamp_utc"),
        get("from"),
        get("to"),
        get("topic"),
        get("body"),
        get("priority"),
        get("tags"),
    ]
}

// ---------------------------------------------------------------------------
// Pure functions (mirror redis_bus.rs LZ4 compression)
// ---------------------------------------------------------------------------

use base64::Engine as _;

fn lz4_compress_body(body: &str) -> (String, usize) {
    let compressed = lz4_flex::compress_prepend_size(body.as_bytes());
    let encoded = base64::engine::general_purpose::STANDARD.encode(&compressed);
    (encoded, body.len())
}

fn lz4_decompress_body(encoded: &str) -> String {
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .expect("base64 decode");
    let raw = lz4_flex::decompress_size_prepended(&compressed).expect("lz4 decompress");
    String::from_utf8(raw).expect("utf-8")
}

// ---------------------------------------------------------------------------
// Test data factories
// ---------------------------------------------------------------------------

fn make_message(body_size: usize) -> Message {
    Message {
        id: "550e8400-e29b-41d4-a716-446655440000".to_owned(),
        timestamp_utc: "2026-03-15T10:30:00.123456Z".to_owned(),
        protocol_version: "1.0".to_owned(),
        from: "claude".to_owned(),
        to: "codex".to_owned(),
        topic: "rust-findings".to_owned(),
        body: "x".repeat(body_size),
        thread_id: Some("thread-abc-123".to_owned()),
        tags: vec![
            "repo:agent-hub".to_owned(),
            "severity:high".to_owned(),
            "component:redis".to_owned(),
        ],
        priority: "high".to_owned(),
        request_ack: true,
        reply_to: Some("claude".to_owned()),
        metadata: serde_json::json!({"_schema": "finding", "session": "perf-test"}),
        stream_id: Some("1710504600123-0".to_owned()),
    }
}

fn make_presence() -> Presence {
    Presence {
        agent: "claude".to_owned(),
        status: "online".to_owned(),
        protocol_version: "1.0".to_owned(),
        timestamp_utc: "2026-03-15T10:30:00.123456Z".to_owned(),
        session_id: "session-550e8400".to_owned(),
        capabilities: vec![
            "orchestration".to_owned(),
            "mcp".to_owned(),
            "redis".to_owned(),
        ],
        metadata: serde_json::json!({"service": "agent-bus", "startup": true}),
        ttl_seconds: 7200,
    }
}

fn make_health() -> Health {
    Health {
        ok: true,
        protocol_version: "1.0".to_owned(),
        redis_url: "redis://localhost:6380/0".to_owned(),
        database_url: Some("postgresql://***@localhost:5300/redis_backend".to_owned()),
        database_ok: Some(true),
        database_error: None,
        storage_ready: true,
        runtime: "rust-native".to_owned(),
        codec: "serde_json".to_owned(),
        stream_length: Some(5423),
        pg_message_count: Some(4892),
        pg_presence_count: Some(312),
    }
}

// ---------------------------------------------------------------------------
// Benchmark 1: TOON encoding
// ---------------------------------------------------------------------------

fn bench_toon_encoding(c: &mut Criterion) {
    let mut group = c.benchmark_group("toon_encoding");

    let msg = make_message(80);
    group.bench_function("message_short_body", |b| {
        b.iter(|| format_message_toon(&msg));
    });

    let msg_long = make_message(500);
    group.bench_function("message_long_body_truncated", |b| {
        b.iter(|| format_message_toon(&msg_long));
    });

    let msg_no_tags = {
        let mut m = make_message(80);
        m.tags.clear();
        m
    };
    group.bench_function("message_no_tags", |b| {
        b.iter(|| format_message_toon(&msg_no_tags));
    });

    let presence = make_presence();
    group.bench_function("presence", |b| {
        b.iter(|| format_presence_toon(&presence));
    });

    let health = make_health();
    group.bench_function("health", |b| {
        b.iter(|| format_health_toon(&health));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 2: LZ4 compression round-trip
// ---------------------------------------------------------------------------

fn bench_lz4_compression(c: &mut Criterion) {
    let mut group = c.benchmark_group("lz4_compression");

    for size in [256, 512, 1024, 4096, 16384] {
        // Build a body of exactly `size` bytes using ASCII-only content.
        let pattern = "a]b[c{d}e:f,g ";
        let repeats = (size / pattern.len()) + 2;
        let body_full = pattern.repeat(repeats);
        let body = &body_full[..size];

        group.throughput(Throughput::Bytes(size as u64));

        group.bench_with_input(BenchmarkId::new("compress", size), &body, |b, body| {
            b.iter(|| lz4_compress_body(body));
        });

        let (compressed, _) = lz4_compress_body(body);
        group.bench_with_input(
            BenchmarkId::new("decompress", size),
            &compressed,
            |b, compressed| {
                b.iter(|| lz4_decompress_body(compressed));
            },
        );

        group.bench_with_input(BenchmarkId::new("round_trip", size), &body, |b, body| {
            b.iter(|| {
                let (compressed, _) = lz4_compress_body(body);
                lz4_decompress_body(&compressed)
            });
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 3: Schema inference
// ---------------------------------------------------------------------------

fn bench_schema_inference(c: &mut Criterion) {
    let mut group = c.benchmark_group("schema_inference");

    let topics = [
        ("rust-findings", "finding topic"),
        ("review-http-rs", "review topic"),
        ("status", "status topic"),
        ("ownership", "ownership topic"),
        ("benchmark", "benchmark topic"),
        ("random-topic", "no-match topic"),
        ("coordination", "coordination topic"),
    ];

    for (topic, label) in &topics {
        group.bench_with_input(BenchmarkId::new("infer", label), topic, |b, topic| {
            b.iter(|| infer_schema_from_topic(topic, None));
        });
    }

    group.bench_function("with_explicit_override", |b| {
        b.iter(|| infer_schema_from_topic("random-topic", Some("finding")));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 4: Message serialization / deserialization
// ---------------------------------------------------------------------------

fn bench_message_serde(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_serde");

    for body_size in [64, 256, 1024, 4096] {
        let msg = make_message(body_size);

        group.throughput(Throughput::Bytes(body_size as u64));

        group.bench_with_input(BenchmarkId::new("serialize", body_size), &msg, |b, msg| {
            b.iter(|| serde_json::to_string(msg).unwrap());
        });

        let json = serde_json::to_string(&msg).unwrap();
        group.bench_with_input(
            BenchmarkId::new("deserialize", body_size),
            &json,
            |b, json| {
                b.iter(|| serde_json::from_str::<Message>(json).unwrap());
            },
        );

        // Serialize to Value then minimize (the Minimal encoding path)
        group.bench_with_input(
            BenchmarkId::new("serialize_minimize", body_size),
            &msg,
            |b, msg| {
                b.iter(|| {
                    let value = serde_json::to_value(msg).unwrap();
                    let minimized = minimize_value(&value);
                    serde_json::to_string(&minimized).unwrap()
                });
            },
        );
    }

    // Batch serialization: 10 messages (typical batch)
    let batch: Vec<Message> = (0..10).map(|_| make_message(256)).collect();
    group.bench_function("serialize_batch_10", |b| {
        b.iter(|| serde_json::to_string(&batch).unwrap());
    });

    let batch_json = serde_json::to_string(&batch).unwrap();
    group.bench_function("deserialize_batch_10", |b| {
        b.iter(|| serde_json::from_str::<Vec<Message>>(&batch_json).unwrap());
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 5: Channel key generation
// ---------------------------------------------------------------------------

fn bench_channel_keys(c: &mut Criterion) {
    let mut group = c.benchmark_group("channel_keys");

    group.bench_function("direct_key_sorted", |b| {
        b.iter(|| direct_key("claude", "codex"));
    });

    group.bench_function("direct_key_reversed", |b| {
        b.iter(|| direct_key("zephyr", "alpha"));
    });

    group.bench_function("group_stream_key", |b| {
        b.iter(|| group_stream_key("review-http-rs"));
    });

    group.bench_function("group_members_key", |b| {
        b.iter(|| group_members_key("review-http-rs"));
    });

    group.bench_function("claims_key_forward_slash", |b| {
        b.iter(|| claims_key("src/redis_bus.rs"));
    });

    group.bench_function("claims_key_backslash", |b| {
        b.iter(|| claims_key("src\\redis_bus.rs"));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 6: UUID generation (called per message)
// ---------------------------------------------------------------------------

fn bench_uuid_generation(c: &mut Criterion) {
    c.bench_function("uuid_v4_to_string", |b| {
        b.iter(|| uuid::Uuid::new_v4().to_string());
    });
}

// ---------------------------------------------------------------------------
// Benchmark 7: Timestamp formatting (called per message)
// ---------------------------------------------------------------------------

fn bench_timestamp_format(c: &mut Criterion) {
    c.bench_function("chrono_utc_format", |b| {
        b.iter(|| {
            chrono::Utc::now()
                .format("%Y-%m-%dT%H:%M:%S%.6fZ")
                .to_string()
        });
    });
}

// ---------------------------------------------------------------------------
// Benchmark 8: Presence serde round-trip
// ---------------------------------------------------------------------------

fn bench_presence_serde(c: &mut Criterion) {
    let mut group = c.benchmark_group("presence_serde");

    let p = make_presence();
    let json = serde_json::to_string(&p).unwrap();

    group.bench_function("serialize", |b| {
        b.iter(|| serde_json::to_string(&p).unwrap());
    });

    group.bench_function("deserialize", |b| {
        b.iter(|| serde_json::from_str::<Presence>(&json).unwrap());
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 9: decode_stream_entry field extraction (Hotpath 2)
// ---------------------------------------------------------------------------

fn bench_decode_stream_fields(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_stream_fields");

    // Simulate 8 ASCII-only fields as they arrive from Redis BulkString bytes.
    let fields: std::collections::HashMap<&str, Vec<u8>> = [
        ("id", b"550e8400-e29b-41d4-a716-446655440000".to_vec()),
        ("timestamp_utc", b"2026-03-15T10:30:00.123456Z".to_vec()),
        ("from", b"claude".to_vec()),
        ("to", b"codex".to_vec()),
        ("topic", b"rust-findings".to_vec()),
        (
            "body",
            b"Reviewing redis_bus.rs hotpath: found excessive allocations in decode_stream_entry"
                .to_vec(),
        ),
        ("priority", b"high".to_vec()),
        (
            "tags",
            br#"["repo:agent-hub","severity:high","component:redis"]"#.to_vec(),
        ),
    ]
    .into_iter()
    .collect();

    group.bench_function("original_lossy_to_string", |b| {
        b.iter(|| decode_fields_original(&fields));
    });

    group.bench_function("optimized_utf8_fast_path", |b| {
        b.iter(|| decode_fields_optimized(&fields));
    });

    // Also benchmark just the single-field extraction to isolate the difference.
    let field_bytes = b"claude".to_vec();
    group.bench_function("single_field_original", |b| {
        b.iter(|| get_field_original(&field_bytes));
    });
    group.bench_function("single_field_optimized", |b| {
        b.iter(|| get_field_optimized(&field_bytes));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion groups
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_toon_encoding,
    bench_lz4_compression,
    bench_schema_inference,
    bench_message_serde,
    bench_channel_keys,
    bench_uuid_generation,
    bench_timestamp_format,
    bench_presence_serde,
    bench_decode_stream_fields,
);

criterion_main!(benches);
