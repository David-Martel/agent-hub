//! Encoding comparison benchmarks for agent-bus-core.
//!
//! Creates a realistic set of 50 messages (various topics, tags, severities,
//! body lengths) and formats them through each encoding mode (JSON, compact,
//! minimal, TOON, human).  Measures byte size of each encoding and prints a
//! comparison table.  Also benchmarks the formatting throughput.

#![allow(clippy::pedantic, clippy::restriction)]

use std::fmt::Write as _;

use criterion::{Criterion, criterion_group, criterion_main};

use agent_bus_core::models::Message;
use agent_bus_core::output::{encode_msgpack, format_message_toon, minimize_value, shorten_tags};

// ---------------------------------------------------------------------------
// Realistic message corpus
// ---------------------------------------------------------------------------

/// Build a corpus of 50 messages that simulate a realistic multi-agent session.
fn make_corpus() -> Vec<Message> {
    let agents = ["claude", "codex", "gemini", "copilot", "euler"];
    let topics = [
        "rust-findings",
        "status",
        "coordination",
        "ownership",
        "benchmark",
        "review-http-rs",
        "escalation",
    ];
    let priorities = ["low", "normal", "normal", "high", "urgent"];
    let severities = ["LOW", "MEDIUM", "HIGH", "CRITICAL"];
    let bodies = [
        "FINDING: excessive allocations in decode_stream_entry hot path\nSEVERITY: HIGH\nFix: use SmallVec<[String; 4]> for tags",
        "Session announced: framework-v0.4 sprint started",
        "COMPLETE: code review finished with 3 findings, 0 blockers",
        "Claiming file src/redis_bus.rs for refactoring batch writes",
        "latency_p99:12ms throughput:4500msg/s memory_peak:48MB",
        "FINDING: missing error context on Redis connection failure\nSEVERITY: MEDIUM\nFix: add .context() to connect()",
        "Ready for review. All tests passing, clippy clean.",
        "FIX applied: replaced unwrap() with proper error propagation in 3 call sites",
        "Escalating: PostgreSQL circuit breaker tripping repeatedly, need DBA review",
        "FINDING: glob re-export in channels.rs exposes internal types\nSEVERITY: LOW\nFix: use explicit re-exports",
    ];

    let mut corpus = Vec::with_capacity(50);
    for i in 0..50 {
        let from = agents[i % agents.len()];
        let to = if i % 7 == 0 {
            "all"
        } else {
            agents[(i + 1) % agents.len()]
        };
        let topic = topics[i % topics.len()];
        let priority = priorities[i % priorities.len()];
        let body = bodies[i % bodies.len()];
        let severity = severities[i % severities.len()];

        let mut tags: smallvec::SmallVec<[String; 4]> = smallvec::SmallVec::new();
        tags.push("repo:agent-bus".to_owned());
        tags.push(format!("session:sprint-{}", 40 + (i / 10)));
        if i % 3 == 0 {
            tags.push(format!("severity:{severity}"));
        }
        if i % 5 == 0 {
            tags.push(format!("thread_id:thread-{}", i / 5));
        }

        let thread_id = if i % 4 == 0 {
            Some(format!("thread-{}", i / 4))
        } else {
            None
        };

        let msg = Message {
            id: format!("msg-{i:04}"),
            timestamp_utc: format!("2026-04-01T10:{:02}:{:02}.000000Z", i % 60, (i * 7) % 60),
            protocol_version: "1.0".to_owned(),
            from: from.to_owned(),
            to: to.to_owned(),
            topic: topic.to_owned(),
            body: body.to_owned(),
            thread_id,
            tags,
            priority: priority.to_owned(),
            request_ack: i % 6 == 0,
            reply_to: if i % 8 == 0 {
                Some(from.to_owned())
            } else {
                None
            },
            metadata: if i % 3 == 0 {
                serde_json::json!({"_schema": "finding"})
            } else {
                serde_json::json!({})
            },
            stream_id: Some(format!("1711929600{i:03}-0")),
        };
        corpus.push(msg);
    }
    corpus
}

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

/// Format a message as pretty-printed JSON (Encoding::Json).
fn format_json(msg: &Message) -> String {
    serde_json::to_string_pretty(msg).unwrap_or_default()
}

/// Format a message as compact JSON (Encoding::Compact).
fn format_compact(msg: &Message) -> String {
    serde_json::to_string(msg).unwrap_or_default()
}

/// Format a message as minimized JSON (Encoding::Minimal) with tag shortening.
fn format_minimal(msg: &Message) -> String {
    let value = serde_json::to_value(msg).unwrap_or_default();
    let minimized = minimize_value(&value);
    serde_json::to_string(&minimized).unwrap_or_default()
}

/// Format a message as a human-readable line (Encoding::Human).
fn format_human(msg: &Message) -> String {
    format!(
        "[{}] {} -> {} | {} | {} | {}",
        msg.timestamp_utc, msg.from, msg.to, msg.topic, msg.priority, msg.body,
    )
}

/// Format a message as TOON (Encoding::Toon).
fn format_toon(msg: &Message) -> String {
    format_message_toon(msg)
}

/// Format a message as MessagePack bytes.
fn format_msgpack(msg: &Message) -> Vec<u8> {
    let value = serde_json::to_value(msg).unwrap_or_default();
    encode_msgpack(&value).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Benchmark: encoding throughput for 50-message corpus
// ---------------------------------------------------------------------------

fn bench_encoding_throughput(c: &mut Criterion) {
    let corpus = make_corpus();

    let mut group = c.benchmark_group("encoding_50msg_throughput");

    group.bench_function("json_pretty", |b| {
        b.iter(|| {
            let _: Vec<String> = corpus.iter().map(format_json).collect();
        });
    });

    group.bench_function("compact_json", |b| {
        b.iter(|| {
            let _: Vec<String> = corpus.iter().map(format_compact).collect();
        });
    });

    group.bench_function("minimal", |b| {
        b.iter(|| {
            let _: Vec<String> = corpus.iter().map(format_minimal).collect();
        });
    });

    group.bench_function("toon", |b| {
        b.iter(|| {
            let _: Vec<String> = corpus.iter().map(format_toon).collect();
        });
    });

    group.bench_function("human", |b| {
        b.iter(|| {
            let _: Vec<String> = corpus.iter().map(format_human).collect();
        });
    });

    group.bench_function("msgpack", |b| {
        b.iter(|| {
            let _: Vec<Vec<u8>> = corpus.iter().map(format_msgpack).collect();
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Benchmark: byte-size comparison (runs once, prints table)
// ---------------------------------------------------------------------------

fn bench_encoding_size_comparison(c: &mut Criterion) {
    let corpus = make_corpus();

    // Compute total byte sizes for each encoding.
    let json_total: usize = corpus.iter().map(|m| format_json(m).len()).sum();
    let compact_total: usize = corpus.iter().map(|m| format_compact(m).len()).sum();
    let minimal_total: usize = corpus.iter().map(|m| format_minimal(m).len()).sum();
    let toon_total: usize = corpus.iter().map(|m| format_toon(m).len()).sum();
    let human_total: usize = corpus.iter().map(|m| format_human(m).len()).sum();
    let msgpack_total: usize = corpus.iter().map(|m| format_msgpack(m).len()).sum();

    // Also measure the effect of tag shortening in isolation.
    let tags_original: usize = corpus
        .iter()
        .flat_map(|m| m.tags.iter())
        .map(|t| t.len())
        .sum();
    let tags_shortened: usize = corpus
        .iter()
        .flat_map(|m| {
            let owned: Vec<String> = m.tags.iter().cloned().collect();
            shorten_tags(&owned)
        })
        .map(|t| t.len())
        .sum();

    // Print comparison table to stderr so it appears during `cargo bench`.
    let mut table = String::with_capacity(512);
    let _ = writeln!(table);
    let _ = writeln!(
        table,
        "  Encoding Byte-Size Comparison (50 realistic messages)"
    );
    let _ = writeln!(table, "  {:-<60}", "");
    let _ = writeln!(
        table,
        "  {:15} {:>10} {:>10}",
        "Encoding", "Bytes", "vs JSON"
    );
    let _ = writeln!(table, "  {:-<60}", "");

    let entries = [
        ("JSON (pretty)", json_total),
        ("Compact JSON", compact_total),
        ("Minimal", minimal_total),
        ("TOON", toon_total),
        ("Human", human_total),
        ("MessagePack", msgpack_total),
    ];
    for (name, bytes) in &entries {
        let pct = if *bytes > 0 && json_total > 0 {
            (*bytes as f64 / json_total as f64) * 100.0
        } else {
            0.0
        };
        let _ = writeln!(table, "  {:15} {:>10} {:>9.1}%", name, bytes, pct);
    }

    let _ = writeln!(table, "  {:-<60}", "");
    let _ = writeln!(
        table,
        "  Tag shortening: {} -> {} bytes ({:.1}% reduction)",
        tags_original,
        tags_shortened,
        if tags_original > 0 {
            (1.0 - tags_shortened as f64 / tags_original as f64) * 100.0
        } else {
            0.0
        }
    );
    let _ = writeln!(table);

    eprintln!("{table}");

    // Wrap the size computation in a trivial benchmark so Criterion still
    // reports something useful.
    let mut group = c.benchmark_group("encoding_50msg_bytesize");
    group.bench_function("compute_sizes", |b| {
        b.iter(|| {
            let _j: usize = corpus.iter().map(|m| format_compact(m).len()).sum();
            let _t: usize = corpus.iter().map(|m| format_toon(m).len()).sum();
            let _m: usize = corpus.iter().map(|m| format_msgpack(m).len()).sum();
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Criterion groups
// ---------------------------------------------------------------------------

criterion_group!(
    benches,
    bench_encoding_throughput,
    bench_encoding_size_comparison,
);
criterion_main!(benches);
