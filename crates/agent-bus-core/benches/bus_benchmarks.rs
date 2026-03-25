//! Criterion benchmarks for agent-bus-core hotpath functions.
//!
//! Measures algorithmic cost of encoding, compression, schema inference,
//! and serialization paths in the core crate.

use base64::Engine as _;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use agent_bus_core::models::{Health, Message, Presence};
use agent_bus_core::output::{
    decode_msgpack, encode_msgpack, format_health_toon, format_message_toon, format_presence_toon,
    minimize_value,
};
use agent_bus_core::validation::infer_schema_from_topic;

// ---------------------------------------------------------------------------
// Local helpers (channel key functions — not yet in core public API)
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
    use serde_json::json;
    let json = json!({
        "id": "550e8400-e29b-41d4-a716-446655440000",
        "timestamp_utc": "2026-03-15T10:30:00.123456Z",
        "protocol_version": "1.0",
        "from": "claude",
        "to": "codex",
        "topic": "rust-findings",
        "body": "x".repeat(body_size),
        "thread_id": "thread-abc-123",
        "tags": ["repo:agent-hub", "severity:high", "component:redis"],
        "priority": "high",
        "request_ack": true,
        "reply_to": "claude",
        "metadata": {"_schema": "finding", "session": "perf-test"},
        "stream_id": "1710504600123-0",
    });
    serde_json::from_value(json).expect("make_message")
}

fn make_presence() -> Presence {
    use serde_json::json;
    let json = json!({
        "agent": "claude",
        "status": "online",
        "protocol_version": "1.0",
        "timestamp_utc": "2026-03-15T10:30:00.123456Z",
        "session_id": "session-550e8400",
        "capabilities": ["orchestration", "mcp", "redis"],
        "metadata": {"service": "agent-bus", "startup": true},
        "ttl_seconds": 7200,
    });
    serde_json::from_value(json).expect("make_presence")
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
        pg_writes_queued: Some(4892),
        pg_writes_completed: Some(4890),
        pg_batches: Some(48),
        pg_write_errors: Some(0),
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

    let msg_no_tags: Message = {
        let mut json = serde_json::to_value(make_message(80)).unwrap();
        json["tags"] = serde_json::json!([]);
        serde_json::from_value(json).unwrap()
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

    for size in [256usize, 512, 1024, 4096, 16384] {
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

    for body_size in [64usize, 256, 1024, 4096] {
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

        let json_val = serde_json::to_value(&msg).unwrap();
        group.bench_with_input(
            BenchmarkId::new("serialize_minimize", body_size),
            &json_val,
            |b, val| {
                b.iter(|| {
                    let minimized = minimize_value(val);
                    serde_json::to_string(&minimized).unwrap()
                });
            },
        );
    }

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
// Benchmark 10: minimize_value
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
// Benchmark 11: MessagePack encode / decode round-trip
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
// Redis availability guard
// ---------------------------------------------------------------------------

fn redis_available() -> bool {
    redis::Client::open("redis://localhost:6380/0")
        .and_then(|c| c.get_connection())
        .is_ok()
}

// ---------------------------------------------------------------------------
// Helpers for Redis benchmarks
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BenchSettings {
    redis_url: String,
    stream_key: String,
    channel_key: String,
    #[expect(dead_code, reason = "reserved for presence benchmarks in future")]
    presence_prefix: String,
    stream_maxlen: u64,
}

fn bench_settings() -> BenchSettings {
    BenchSettings {
        redis_url: "redis://localhost:6380/0".to_owned(),
        stream_key: "agent_bus:bench:messages".to_owned(),
        channel_key: "agent_bus:bench:events".to_owned(),
        presence_prefix: "agent_bus:bench:presence:".to_owned(),
        stream_maxlen: 10_000,
    }
}

fn open_conn(s: &BenchSettings) -> redis::Connection {
    redis::Client::open(s.redis_url.as_str())
        .expect("Redis client")
        .get_connection()
        .expect("Redis connection")
}

fn xadd_one(conn: &mut redis::Connection, s: &BenchSettings, from: &str, to: &str) -> String {
    use redis::Commands as _;
    let id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string();
    let tags_json = r#"["bench:true"]"#;
    let meta_json = "{}";

    let stream_id: String = redis::cmd("XADD")
        .arg(&s.stream_key)
        .arg("MAXLEN")
        .arg("~")
        .arg(s.stream_maxlen)
        .arg("*")
        .arg(&[
            ("id", id.as_str()),
            ("timestamp_utc", ts.as_str()),
            ("protocol_version", "1.0"),
            ("from", from),
            ("to", to),
            ("topic", "bench"),
            (
                "body",
                "benchmark payload — measuring real Redis round-trip",
            ),
            ("tags", tags_json),
            ("priority", "normal"),
            ("request_ack", "false"),
            ("reply_to", from),
            ("metadata", meta_json),
        ])
        .query(conn)
        .expect("XADD");

    let event = format!(r#"{{"event":"message","from":"{from}","to":"{to}"}}"#);
    let _: i64 = conn.publish(&s.channel_key, &event).unwrap_or(0);

    stream_id
}

fn xadd_batch(conn: &mut redis::Connection, s: &BenchSettings, count: usize) -> Vec<String> {
    let ids: Vec<String> = (0..count)
        .map(|_| uuid::Uuid::new_v4().to_string())
        .collect();
    let ts = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string();

    let mut pipe = redis::pipe();
    for id in &ids {
        pipe.cmd("XADD")
            .arg(&s.stream_key)
            .arg("MAXLEN")
            .arg("~")
            .arg(s.stream_maxlen)
            .arg("*")
            .arg(&[
                ("id", id.as_str()),
                ("timestamp_utc", ts.as_str()),
                ("protocol_version", "1.0"),
                ("from", "bench-sender"),
                ("to", "bench-recv"),
                ("topic", "bench"),
                ("body", "batch benchmark payload"),
                ("tags", r#"["bench:true"]"#),
                ("priority", "normal"),
                ("request_ack", "false"),
                ("reply_to", "bench-sender"),
                ("metadata", "{}"),
            ]);
    }
    pipe.query::<Vec<String>>(conn).expect("pipeline XADD")
}

fn xrevrange(conn: &mut redis::Connection, s: &BenchSettings, count: usize) -> usize {
    let raw: Vec<redis::Value> = redis::cmd("XREVRANGE")
        .arg(&s.stream_key)
        .arg("+")
        .arg("-")
        .arg("COUNT")
        .arg(count)
        .query(conn)
        .expect("XREVRANGE");
    raw.len()
}

fn seed_stream(conn: &mut redis::Connection, s: &BenchSettings, n: usize) {
    if n == 0 {
        return;
    }
    let mut pipe = redis::pipe();
    for i in 0..n {
        let id = format!("bench-seed-{i}");
        let ts = "2026-03-20T10:00:00.000000Z";
        pipe.cmd("XADD")
            .arg(&s.stream_key)
            .arg("MAXLEN")
            .arg("~")
            .arg(s.stream_maxlen)
            .arg("*")
            .arg(&[
                ("id", id.as_str()),
                ("timestamp_utc", ts),
                ("protocol_version", "1.0"),
                ("from", "bench-seeder"),
                ("to", "bench-consumer"),
                ("topic", "bench-seed"),
                ("body", "seed message for read benchmarks"),
                ("tags", r#"["bench:seed"]"#),
                ("priority", "normal"),
                ("request_ack", "false"),
                ("reply_to", "bench-seeder"),
                ("metadata", "{}"),
            ]);
    }
    let _: Vec<String> = pipe.query(conn).unwrap_or_default();
}

// ---------------------------------------------------------------------------
// Benchmark 12: Redis post_message
// ---------------------------------------------------------------------------

fn bench_redis_post_message(c: &mut Criterion) {
    if !redis_available() {
        eprintln!(
            "[bench_redis_post_message] SKIP — Redis not available at redis://localhost:6380/0"
        );
        return;
    }

    let s = bench_settings();
    let mut conn = open_conn(&s);
    let sender = format!("bench-post-{}", uuid::Uuid::new_v4());

    let mut group = c.benchmark_group("redis_post_message");
    group.measurement_time(std::time::Duration::from_secs(5));
    group.sample_size(50);

    group.bench_function("xadd_and_publish", |b| {
        b.iter(|| xadd_one(&mut conn, &s, &sender, "bench-recv"));
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 13: Redis list_messages
// ---------------------------------------------------------------------------

fn bench_redis_list_messages(c: &mut Criterion) {
    if !redis_available() {
        eprintln!(
            "[bench_redis_list_messages] SKIP — Redis not available at redis://localhost:6380/0"
        );
        return;
    }

    let s = bench_settings();
    let mut conn = open_conn(&s);
    seed_stream(&mut conn, &s, 1_000);

    let mut group = c.benchmark_group("redis_list_messages");
    group.measurement_time(std::time::Duration::from_secs(5));
    group.sample_size(30);

    for count in [10usize, 100, 1_000] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::new("xrevrange", count), &count, |b, &count| {
            b.iter(|| xrevrange(&mut conn, &s, count));
        });
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 14: Redis batch send
// ---------------------------------------------------------------------------

fn bench_redis_batch_send(c: &mut Criterion) {
    if !redis_available() {
        eprintln!(
            "[bench_redis_batch_send] SKIP — Redis not available at redis://localhost:6380/0"
        );
        return;
    }

    let s = bench_settings();
    let mut conn = open_conn(&s);

    let mut group = c.benchmark_group("redis_batch_send");
    group.measurement_time(std::time::Duration::from_secs(5));
    group.sample_size(30);

    for count in [10usize, 50, 100] {
        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(
            BenchmarkId::new("pipeline_xadd", count),
            &count,
            |b, &count| {
                b.iter(|| xadd_batch(&mut conn, &s, count));
            },
        );
    }

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark 15: check_inbox cursor read
// ---------------------------------------------------------------------------

fn bench_redis_check_inbox(c: &mut Criterion) {
    if !redis_available() {
        eprintln!(
            "[bench_redis_check_inbox] SKIP — Redis not available at redis://localhost:6380/0"
        );
        return;
    }

    let s = bench_settings();
    let mut conn = open_conn(&s);
    seed_stream(&mut conn, &s, 100);

    let first_id: String = {
        let raw: Vec<redis::Value> = redis::cmd("XRANGE")
            .arg(&s.stream_key)
            .arg("-")
            .arg("+")
            .arg("COUNT")
            .arg(1)
            .query(&mut conn)
            .unwrap_or_default();
        if let Some(redis::Value::Array(parts)) = raw.first()
            && let Some(redis::Value::BulkString(id_bytes)) = parts.first()
        {
            String::from_utf8_lossy(id_bytes).to_string()
        } else {
            "0-0".to_owned()
        }
    };

    let mut group = c.benchmark_group("redis_check_inbox");
    group.measurement_time(std::time::Duration::from_secs(5));
    group.sample_size(50);

    group.bench_function("xrange_from_origin", |b| {
        b.iter(|| {
            let exclusive_start = format!("({first_id}");
            let _raw: Vec<redis::Value> = redis::cmd("XRANGE")
                .arg(&s.stream_key)
                .arg(&exclusive_start)
                .arg("+")
                .arg("COUNT")
                .arg(10)
                .query(&mut conn)
                .unwrap_or_default();
        });
    });

    let cursor_key = format!("bus:cursor:bench-inbox-{}", uuid::Uuid::new_v4());
    group.bench_function("full_cursor_read_advance", |b| {
        b.iter(|| {
            let cursor: String = redis::cmd("GET")
                .arg(&cursor_key)
                .query(&mut conn)
                .unwrap_or_else(|_| "0-0".to_owned());

            let exclusive = format!("({cursor}");
            let raw: Vec<redis::Value> = redis::cmd("XRANGE")
                .arg(&s.stream_key)
                .arg(&exclusive)
                .arg("+")
                .arg("COUNT")
                .arg(10)
                .query(&mut conn)
                .unwrap_or_default();

            if let Some(redis::Value::Array(parts)) = raw.last()
                && let Some(redis::Value::BulkString(id_bytes)) = parts.first()
            {
                let new_cursor = String::from_utf8_lossy(id_bytes).to_string();
                let _: () = redis::cmd("SET")
                    .arg(&cursor_key)
                    .arg(&new_cursor)
                    .query(&mut conn)
                    .unwrap_or(());
            }
        });
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
    bench_minimize_value,
    bench_msgpack,
);

criterion_group!(
    redis_benches,
    bench_redis_post_message,
    bench_redis_list_messages,
    bench_redis_batch_send,
    bench_redis_check_inbox,
);

criterion_main!(benches, redis_benches);
