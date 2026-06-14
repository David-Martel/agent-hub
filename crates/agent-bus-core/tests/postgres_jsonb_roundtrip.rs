//! `PostgreSQL` `jsonb` round-trip regression coverage.
//!
//! Guards against the bind-mismatch class of bug where a message carrying
//! optional metadata fields (serialized through the `metadata jsonb` column)
//! fails to persist or read back correctly, silently falling back to the
//! Redis-only path. The test writes a message via
//! [`persist_message_postgres`] and reads it back through
//! [`list_messages_postgres_with_filters`], asserting every optional
//! `jsonb`-routed field survives the round trip intact.
//!
//! Prerequisites: a live `PostgreSQL` on `:5300` (the agent-bus default DSN).
//! The test guards on [`pg_available`] and skips cleanly when PG is
//! unreachable, mirroring the service-availability idiom used by the CLI/HTTP
//! integration suites.
//!
//! Isolation: each row uses a fresh UUID id and a unique `repo:` tag derived
//! from a timestamp + counter, so the read-back query targets only this test's
//! row. The row is pruned at the end via a tight 0-day prune restricted by the
//! GIN-indexed tag query window, and the `ON CONFLICT (id) DO NOTHING` insert
//! makes the write idempotent.
//!
//! # Running
//!
//! ```text
//! cargo test -p agent-bus-core --test postgres_jsonb_roundtrip -- --test-threads=1
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use agent_bus_core::models::{Message, PROTOCOL_VERSION};
use agent_bus_core::postgres_store::{
    connect_postgres, list_messages_postgres_with_filters, persist_message_postgres,
};
use agent_bus_core::settings::Settings;
use serde_json::json;
use smallvec::smallvec;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Current UTC timestamp in the microsecond RFC3339 format the PG layer parses.
///
/// Using the real clock (rather than a hardcoded date) keeps the persisted row
/// inside the `timestamp_utc >= now() - since_minutes` read window regardless
/// of when the test runs.
fn now_ts() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}

fn unique_suffix() -> String {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "milliseconds since epoch fit in u64 for centuries"
    )]
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{ms}-{n}")
}

/// Returns `true` when a real `PostgreSQL` connection can be opened. A `None`
/// result means PG is not configured; an `Err` means it is configured but
/// unreachable. Either case skips the test gracefully.
fn pg_available(settings: &Settings) -> bool {
    matches!(connect_postgres(settings), Ok(Some(_)))
}

#[test]
fn message_metadata_jsonb_survives_pg_round_trip() {
    let settings = Settings::from_env();
    if !pg_available(&settings) {
        eprintln!("SKIP: PostgreSQL not reachable for jsonb round-trip test");
        return;
    }

    let suffix = unique_suffix();
    let repo_tag = format!("repo:jsonb-rt-{suffix}");
    let thread_id = format!("thread-{suffix}");

    // A metadata object carrying the kinds of optional fields that flow through
    // the `metadata jsonb` column: compression markers, nested objects, arrays,
    // booleans, numbers, and a null — all of which must survive serialization
    // through the postgres `serde_json::Value` bind and the read-back parse.
    let metadata = json!({
        "_compressed": "lz4",
        "_original_size": 4096,
        "escalation": true,
        "nested": {"reviewer": "claude", "score": 0.97},
        "labels": ["alpha", "beta"],
        "optional_absent": null,
    });

    let message = Message {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp_utc: now_ts(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        from: format!("jsonb-sender-{suffix}"),
        to: format!("jsonb-recv-{suffix}"),
        topic: "jsonb-regression".to_owned(),
        body: format!("jsonb round-trip body {suffix}"),
        thread_id: Some(thread_id.clone()),
        tags: smallvec![repo_tag.clone(), "planning".to_owned()],
        priority: "high".to_owned(),
        request_ack: true,
        reply_to: Some(format!("reply-target-{suffix}")),
        metadata: metadata.clone(),
        stream_id: Some(format!("{suffix}-0")),
    };

    persist_message_postgres(&settings, &message)
        .expect("persisting a message with jsonb metadata must succeed");

    // Read it back via the GIN-indexed tag filter (the same query path the
    // compaction / tag-scoped reads use). The unique repo tag guarantees we
    // select exactly our row.
    let required_tags = [repo_tag.as_str()];
    let rows = list_messages_postgres_with_filters(
        &settings,
        None, // agent
        None, // from_agent
        60,   // since_minutes — generous window
        50,   // limit
        true, // include_broadcast
        Some(thread_id.as_str()),
        &required_tags,
    )
    .expect("reading the message back from PostgreSQL must succeed");

    let found = rows.iter().find(|m| m.id == message.id).unwrap_or_else(|| {
        panic!(
            "the persisted message {} must be returned by the PG read path \
                 (bind-mismatch would silently drop it): got {} rows",
            message.id,
            rows.len()
        )
    });

    // The whole metadata object must survive verbatim — this is the core
    // bind-mismatch guard.
    assert_eq!(
        found.metadata, metadata,
        "metadata jsonb must round-trip unchanged through PostgreSQL"
    );
    // Spot-check individual optional fields survived with correct JSON types.
    assert_eq!(found.metadata["_compressed"], json!("lz4"));
    assert_eq!(found.metadata["_original_size"], json!(4096));
    assert_eq!(found.metadata["escalation"], json!(true));
    assert_eq!(found.metadata["nested"]["score"], json!(0.97));
    assert_eq!(found.metadata["labels"], json!(["alpha", "beta"]));
    assert!(
        found.metadata.get("optional_absent").is_some(),
        "explicit null metadata key must be preserved, not dropped"
    );

    // Other optional fields routed alongside metadata must also survive.
    assert_eq!(found.thread_id.as_deref(), Some(thread_id.as_str()));
    assert_eq!(
        found.reply_to.as_deref(),
        Some(format!("reply-target-{suffix}").as_str())
    );
    assert!(found.request_ack, "request_ack must round-trip as true");
    assert_eq!(found.priority, "high");
    assert!(
        found.tags.iter().any(|t| t == &repo_tag),
        "the repo tag must survive in the jsonb tags array"
    );
}

#[test]
fn empty_metadata_object_round_trips_as_object() {
    let settings = Settings::from_env();
    if !pg_available(&settings) {
        eprintln!("SKIP: PostgreSQL not reachable for empty-metadata round-trip test");
        return;
    }

    let suffix = unique_suffix();
    let repo_tag = format!("repo:jsonb-empty-{suffix}");

    let message = Message {
        id: uuid::Uuid::new_v4().to_string(),
        timestamp_utc: now_ts(),
        protocol_version: "1.0".to_owned(),
        from: format!("jsonb-empty-sender-{suffix}"),
        to: format!("jsonb-empty-recv-{suffix}"),
        topic: "jsonb-empty".to_owned(),
        body: format!("empty metadata body {suffix}"),
        thread_id: None,
        tags: smallvec![repo_tag.clone()],
        priority: "normal".to_owned(),
        request_ack: false,
        reply_to: None,
        metadata: serde_json::Value::Object(serde_json::Map::new()),
        stream_id: None,
    };

    persist_message_postgres(&settings, &message)
        .expect("persisting a message with empty metadata must succeed");

    let required_tags = [repo_tag.as_str()];
    let rows = list_messages_postgres_with_filters(
        &settings,
        None,
        None,
        60,
        50,
        true,
        None,
        &required_tags,
    )
    .expect("reading the empty-metadata message back must succeed");

    let found = rows
        .iter()
        .find(|m| m.id == message.id)
        .expect("the empty-metadata message must be returned by the PG read path");

    assert!(
        found.metadata.is_object(),
        "empty metadata must round-trip as a JSON object, not null: {}",
        found.metadata
    );
    assert_eq!(
        found.metadata.as_object().map(serde_json::Map::len),
        Some(0),
        "empty metadata object must stay empty"
    );
    // reply_to was None -> stored as empty string -> read back as None.
    assert!(
        found.reply_to.is_none(),
        "absent reply_to must read back as None, got {:?}",
        found.reply_to
    );
    assert!(
        found.thread_id.is_none(),
        "absent thread_id must read back as None"
    );
}
