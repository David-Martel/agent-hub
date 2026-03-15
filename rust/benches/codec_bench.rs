//! Criterion benchmarks for codec serialization.
//!
//! Measures compact serialization and JSON parsing for small, large,
//! and batch payloads. Baseline for comparing against Python stdlib json.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use serde_json::{json, Value};

fn small_message() -> Value {
    json!({
        "id": "550e8400-e29b-41d4-a716-446655440000",
        "from": "claude",
        "to": "codex",
        "topic": "status",
        "body": "ready",
        "priority": "normal",
        "tags": [],
        "request_ack": false
    })
}

fn large_message() -> Value {
    let body: String = (0..50)
        .map(|i| format!("- Issue {i}: description {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    json!({
        "id": "550e8400-e29b-41d4-a716-446655440000",
        "timestamp_utc": "2026-03-14T12:00:00Z",
        "protocol_version": "1.0",
        "from": "claude:rust-pro",
        "to": "all",
        "topic": "review-findings",
        "body": format!("Found issues:\n{body}"),
        "tags": ["review", "rust-pro", "blood-sugar", "findings", "security"],
        "priority": "high",
        "request_ack": true,
        "reply_to": "claude",
        "metadata": {
            "agent_type": "rust-pro",
            "file_count": 47,
            "findings": {"critical": 2, "high": 8, "medium": 15, "low": 20}
        }
    })
}

fn bench_compact(c: &mut Criterion) {
    let small = small_message();
    let large = large_message();

    c.bench_function("compact_small", |b| {
        b.iter(|| serde_json::to_string(black_box(&small)));
    });

    c.bench_function("compact_large", |b| {
        b.iter(|| serde_json::to_string(black_box(&large)));
    });

    let batch: Vec<Value> = (0..100)
        .map(|i| {
            json!({"id": format!("msg-{i}"), "from": "claude", "to": "codex", "topic": "status", "body": format!("msg {i}")})
        })
        .collect();

    c.bench_function("compact_batch_100", |b| {
        b.iter(|| {
            for msg in black_box(&batch) {
                let _ = serde_json::to_string(msg);
            }
        });
    });
}

fn bench_parse(c: &mut Criterion) {
    let small_json = serde_json::to_string(&small_message()).unwrap();
    let large_json = serde_json::to_string(&large_message()).unwrap();

    c.bench_function("parse_small", |b| {
        b.iter(|| serde_json::from_str::<Value>(black_box(&small_json)));
    });

    c.bench_function("parse_large", |b| {
        b.iter(|| serde_json::from_str::<Value>(black_box(&large_json)));
    });
}

criterion_group!(benches, bench_compact, bench_parse);
criterion_main!(benches);
