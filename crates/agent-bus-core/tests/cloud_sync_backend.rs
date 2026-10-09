//! Backend coverage for the cloud-sync storage primitives (agent-hub#79).
//!
//! Every test is `#[ignore]`d and runs only against DISPOSABLE backends named
//! by `AGENT_BUS_TEST_REDIS_URL` and `AGENT_BUS_TEST_DATABASE_URL` (see
//! `tests/support/backend_env.rs`). An unset variable or an unreachable backend
//! FAILS the test, and the live bus ports 6380, 5300 and 8400 are refused.
//!
//! Isolation: each test uses a unique Redis stream key and presence prefix and
//! its own `PostgreSQL` table names, so nothing here touches a default key.
//!
//! ```text
//! cargo test -p agent-bus-core --test cloud_sync_backend -- --ignored --test-threads=1
//! ```

use std::sync::atomic::{AtomicU64, Ordering};

use agent_bus_core::channels::{ClaimStatus, get_arbitration_state, release_claim, renew_claim};
use agent_bus_core::models::{Message, PROTOCOL_VERSION, Presence, Sensitivity};
use agent_bus_core::postgres_store::{
    connect_postgres, list_messages_postgres_with_filters, list_presence_events_after,
    persist_message_postgres, persist_presence_postgres, presence_event_max_id,
};
use agent_bus_core::redis_bus::connect;
use agent_bus_core::settings::Settings;
use agent_bus_core::sync_store::{
    IngestOutcome, apply_synced_presence, ingest_seen_key, ingest_synced_message,
    read_messages_after,
};
use chrono::Utc;
use smallvec::smallvec;

#[path = "support/backend_env.rs"]
mod backend_env;

use backend_env::{DATABASE_URL_VAR, REDIS_URL_VAR, backend_url};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique() -> String {
    format!(
        "{}-{}",
        uuid::Uuid::new_v4().simple(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_ts() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

/// Settings on the disposable Redis with a private stream and presence prefix.
fn redis_settings(tag: &str) -> Settings {
    let mut settings = Settings::from_env();
    settings.redis_url = backend_url(REDIS_URL_VAR);
    settings.database_url = None;
    settings.stream_key = format!("agent_bus_test:cloudsync:{tag}:messages");
    settings.presence_prefix = format!("agent_bus_test:cloudsync:{tag}:presence:");
    settings
}

fn pg_settings(tag: &str) -> Settings {
    let mut settings = Settings::from_env();
    settings.database_url = Some(backend_url(DATABASE_URL_VAR));
    // A private schema per test: index names are schema-scoped, and the DDL's
    // `create index if not exists` would otherwise skip an index whose name
    // already exists on another table in the same schema.
    let schema = format!("cs_{}", tag.replace('-', "_"));
    settings.message_table = format!("{schema}.messages");
    settings.presence_event_table = format!("{schema}.presence_events");
    match connect_postgres(&settings) {
        Ok(Some(mut client)) => {
            client
                .batch_execute(&format!("create schema {schema}"))
                .expect("could not create the test schema");
            settings
        }
        Ok(None) => panic!("PostgreSQL not configured despite {DATABASE_URL_VAR} being set"),
        Err(e) => panic!("PostgreSQL unreachable via {DATABASE_URL_VAR}: {e}"),
    }
}

/// Drop the test's private schema and everything in it.
fn drop_schema(settings: &Settings) {
    let schema = settings.message_table.split('.').next().unwrap();
    connect_postgres(settings)
        .unwrap()
        .unwrap()
        .batch_execute(&format!("drop schema {schema} cascade"))
        .unwrap();
}

fn message(id: &str, origin_hub: Option<&str>) -> Message {
    Message {
        id: id.to_owned(),
        timestamp_utc: now_ts(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        from: "roamer".to_owned(),
        to: "all".to_owned(),
        topic: "status".to_owned(),
        body: format!("body of {id}"),
        thread_id: Some("thread-1".to_owned()),
        tags: smallvec!["repo:agent-hub".to_owned()],
        priority: "normal".to_owned(),
        request_ack: false,
        reply_to: None,
        metadata: serde_json::json!({"k": "v"}),
        stream_id: None,
        client_msg_id: Some(format!("cm-{id}")),
        origin_hub: origin_hub.map(str::to_owned),
        origin_seq: Some(7),
        hlc: Some("1700000000000-0-cloud".to_owned()),
        sensitivity: Some(Sensitivity::Internal),
    }
}

fn presence(agent: &str, ts: &str, ttl: u64, origin: &str) -> Presence {
    Presence {
        agent: agent.to_owned(),
        status: "online".to_owned(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        timestamp_utc: ts.to_owned(),
        session_id: "s".to_owned(),
        capabilities: vec![],
        metadata: serde_json::json!({ "origin_hub": origin }),
        ttl_seconds: ttl,
    }
}

const REDIS_IGNORE: &str =
    "backend test: needs AGENT_BUS_TEST_REDIS_URL (see tests/support/backend_env.rs)";
const PG_IGNORE: &str =
    "backend test: needs AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)";

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL (see tests/support/backend_env.rs)"]
#[test]
fn ingest_is_idempotent_and_keeps_the_origin_in_the_stream() {
    let _ = REDIS_IGNORE;
    let settings = redis_settings(&unique());
    let mut conn = connect(&settings).expect("disposable Redis unreachable");
    let msg = message("0190a000-0000-7000-8000-000000000001", Some("cloud"));

    assert_eq!(
        ingest_synced_message(&mut conn, &settings, &msg, None).unwrap(),
        IngestOutcome::Ingested
    );
    assert_eq!(
        ingest_synced_message(&mut conn, &settings, &msg, None).unwrap(),
        IngestOutcome::Duplicate,
        "a replay of the same (origin_hub, id) must not write again"
    );
    // The same id from a different origin is a different message.
    let other = message(&msg.id, Some("hub-b"));
    assert_eq!(
        ingest_synced_message(&mut conn, &settings, &other, None).unwrap(),
        IngestOutcome::Ingested
    );

    let read = read_messages_after(&mut conn, &settings, "0-0", 10).unwrap();
    assert_eq!(read.len(), 2, "exactly one stream entry per (origin, id)");
    let first = &read[0];
    assert_eq!(first.id, msg.id);
    assert_eq!(first.origin_hub.as_deref(), Some("cloud"));
    assert_eq!(
        first.client_msg_id.as_deref(),
        Some("cm-0190a000-0000-7000-8000-000000000001")
    );
    assert_eq!(first.origin_seq, Some(7));
    assert_eq!(first.hlc.as_deref(), Some("1700000000000-0-cloud"));
    assert_eq!(first.sensitivity, Some(Sensitivity::Internal));
    assert_eq!(first.metadata, serde_json::json!({"k": "v"}));
    assert_eq!(first.thread_id.as_deref(), Some("thread-1"));

    // Cursor semantics: strictly after the first entry leaves exactly one.
    let after = read_messages_after(
        &mut conn,
        &settings,
        first.stream_id.as_deref().unwrap(),
        10,
    )
    .unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].origin_hub.as_deref(), Some("hub-b"));
    assert!(
        read_messages_after(
            &mut conn,
            &settings,
            after[0].stream_id.as_deref().unwrap(),
            10
        )
        .unwrap()
        .is_empty()
    );

    let _: () = redis::cmd("DEL")
        .arg(&settings.stream_key)
        .arg(ingest_seen_key("cloud", &msg.id))
        .arg(ingest_seen_key("hub-b", &msg.id))
        .query(&mut conn)
        .unwrap();
}

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL (see tests/support/backend_env.rs)"]
#[test]
fn ingest_refuses_a_message_without_an_origin() {
    let settings = redis_settings(&unique());
    let mut conn = connect(&settings).expect("disposable Redis unreachable");
    let msg = message("0190a000-0000-7000-8000-000000000002", None);
    assert!(ingest_synced_message(&mut conn, &settings, &msg, None).is_err());
    assert!(
        read_messages_after(&mut conn, &settings, "0-0", 10)
            .unwrap()
            .is_empty(),
        "nothing may be written for an unattributed message"
    );
}

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL (disposable Redis)"]
#[test]
fn ingest_append_errors_leave_no_marker_and_retries_succeed() {
    let tag = unique();
    let settings = redis_settings(&tag);
    let mut conn = connect(&settings).expect("disposable Redis unreachable");
    let msg = message(&unique(), Some(&tag));
    let seen = ingest_seen_key(&tag, &msg.id);
    // Wrong-type errors are rejected before reserving the marker.
    let _: () = redis::cmd("SET")
        .arg(&settings.stream_key)
        .arg("not a stream")
        .query(&mut conn)
        .unwrap();
    assert!(ingest_synced_message(&mut conn, &settings, &msg, None).is_err());
    assert!(
        !redis::cmd("EXISTS")
            .arg(&seen)
            .query::<bool>(&mut conn)
            .unwrap()
    );
    let _: () = redis::cmd("DEL")
        .arg(&settings.stream_key)
        .query(&mut conn)
        .unwrap();
    // A valid stream at its maximum ID passes TYPE but makes XADD '*' fail.
    let _: String = redis::cmd("XADD")
        .arg(&settings.stream_key)
        .arg("18446744073709551615-18446744073709551615")
        .arg("id")
        .arg("exhausted")
        .query(&mut conn)
        .unwrap();
    assert!(ingest_synced_message(&mut conn, &settings, &msg, None).is_err());
    assert!(
        !redis::cmd("EXISTS")
            .arg(&seen)
            .query::<bool>(&mut conn)
            .unwrap()
    );
    let _: () = redis::cmd("DEL")
        .arg(&settings.stream_key)
        .query(&mut conn)
        .unwrap();
    assert_eq!(
        ingest_synced_message(&mut conn, &settings, &msg, None).unwrap(),
        IngestOutcome::Ingested
    );
    assert_eq!(
        ingest_synced_message(&mut conn, &settings, &msg, None).unwrap(),
        IngestOutcome::Duplicate
    );
    assert_eq!(
        read_messages_after(&mut conn, &settings, "0-0", 10)
            .unwrap()
            .len(),
        1
    );
    let _: () = redis::cmd("DEL")
        .arg(&settings.stream_key)
        .arg(&seen)
        .query(&mut conn)
        .unwrap();
}

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL (disposable Redis)"]
#[test]
fn concurrent_duplicate_ingestions_append_exactly_once() {
    let tag = unique();
    let settings = redis_settings(&tag);
    let msg = message(&unique(), Some(&tag));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let barrier = std::sync::Arc::clone(&barrier);
                let settings = &settings;
                let msg = &msg;
                scope.spawn(move || {
                    let mut conn = connect(settings).expect("disposable Redis unreachable");
                    barrier.wait();
                    ingest_synced_message(&mut conn, settings, msg, None).unwrap()
                })
            })
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect()
    });
    assert_eq!(
        outcomes
            .iter()
            .filter(|&&outcome| outcome == IngestOutcome::Ingested)
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|&&outcome| outcome == IngestOutcome::Duplicate)
            .count(),
        7
    );
    let mut conn = connect(&settings).unwrap();
    assert_eq!(
        read_messages_after(&mut conn, &settings, "0-0", 10)
            .unwrap()
            .len(),
        1
    );
    let _: () = redis::cmd("DEL")
        .arg(&settings.stream_key)
        .arg(ingest_seen_key(&tag, &msg.id))
        .query(&mut conn)
        .unwrap();
}

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL (disposable Redis with ACL admin)"]
#[test]
fn ingest_denied_commands_do_not_poison_replay() {
    let tag = unique();
    let settings = redis_settings(&tag);
    let mut admin = connect(&settings).expect("disposable Redis unreachable");
    let msg = message(&unique(), Some(&tag));
    let seen = ingest_seen_key(&tag, &msg.id);
    let user = format!("cloudsync-test-{tag}");
    let _: () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg(&user)
        .arg("on")
        .arg("nopass")
        .arg("~*")
        .arg("+@all")
        .arg("-xadd")
        .query(&mut admin)
        .expect("disposable Redis must permit ACL test users");
    let mut restricted = connect(&settings).unwrap();
    let _: () = redis::cmd("AUTH")
        .arg(&user)
        .arg("")
        .query(&mut restricted)
        .unwrap();
    assert!(ingest_synced_message(&mut restricted, &settings, &msg, None).is_err());
    assert!(
        !redis::cmd("EXISTS")
            .arg(&seen)
            .query::<bool>(&mut admin)
            .unwrap()
    );
    // Cleanup permission must be checked before SET, even on Redis 6.2.
    let _: () = redis::cmd("ACL")
        .arg("SETUSER")
        .arg(&user)
        .arg("+xadd")
        .arg("-del")
        .query(&mut admin)
        .unwrap();
    assert!(ingest_synced_message(&mut restricted, &settings, &msg, None).is_err());
    assert!(
        !redis::cmd("EXISTS")
            .arg(&seen)
            .query::<bool>(&mut admin)
            .unwrap()
    );
    assert_eq!(
        ingest_synced_message(&mut admin, &settings, &msg, None).unwrap(),
        IngestOutcome::Ingested
    );
    let _: () = redis::cmd("DEL")
        .arg(&settings.stream_key)
        .arg(&seen)
        .query(&mut admin)
        .unwrap();
    let _: u64 = redis::cmd("ACL")
        .arg("DELUSER")
        .arg(&user)
        .query(&mut admin)
        .unwrap();
}

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL (disposable Redis)"]
#[test]
fn legacy_uppercase_claim_can_be_read_renewed_and_released() {
    let settings = redis_settings(&unique());
    let resource = format!("./Legacy/{}/Main.RS", unique());
    let key = format!("bus:claims:{resource}");
    let mut conn = connect(&settings).expect("disposable Redis unreachable");
    let old_claim = serde_json::json!({
        "resource": resource, "agent": "legacy-owner", "priority_argument": "existing lease",
        "timestamp": now_ts(), "status": "granted", "lease_ttl_seconds": 300,
        "expires_at": (Utc::now() + chrono::Duration::seconds(300)).to_rfc3339()
    });
    let _: () = redis::cmd("HSET")
        .arg(&key)
        .arg("legacy-owner")
        .arg(old_claim.to_string())
        .query(&mut conn)
        .unwrap();
    let _: () = redis::cmd("EXPIRE")
        .arg(&key)
        .arg(300)
        .query(&mut conn)
        .unwrap();
    let state = get_arbitration_state(&settings, &resource).unwrap();
    assert_eq!(state.claims.len(), 1);
    assert_eq!(state.claims[0].agent, "legacy-owner");
    assert_eq!(state.claims[0].status, ClaimStatus::Granted);
    assert_eq!(
        renew_claim(&settings, &resource, "legacy-owner", Some(600))
            .unwrap()
            .agent,
        "legacy-owner"
    );
    let history_key = agent_bus_core::redis_bus::resource_event_stream_key(&resource);
    assert_eq!(history_key, format!("agent_bus:resource_events:{resource}"));
    release_claim(&settings, &resource, "legacy-owner").unwrap();
    assert!(
        !redis::cmd("EXISTS")
            .arg(&key)
            .query::<bool>(&mut conn)
            .unwrap()
    );
    let _: () = redis::cmd("DEL")
        .arg(&history_key)
        .query(&mut conn)
        .unwrap();
}

#[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL (see tests/support/backend_env.rs)"]
#[test]
fn pulled_presence_never_overrides_a_newer_local_row() {
    let settings = redis_settings(&unique());
    let mut conn = connect(&settings).expect("disposable Redis unreachable");
    let now = Utc::now();
    let ts = |secs: i64| {
        (now - chrono::Duration::seconds(secs))
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string()
    };

    // No local row: the remote row is written, with the remaining TTL.
    let remote = presence("roamer", &ts(10), 300, "cloud");
    assert!(apply_synced_presence(&mut conn, &settings, &remote, "hub-a", now).unwrap());
    let key = format!("{}roamer", settings.presence_prefix);
    let ttl: i64 = redis::cmd("TTL").arg(&key).query(&mut conn).unwrap();
    assert!(
        (280..=300).contains(&ttl),
        "ttl {ttl} should be the remaining ~290s"
    );

    // A NEWER local row stays.
    let local_key = format!("{}local-agent", settings.presence_prefix);
    let local = presence("local-agent", &ts(1), 300, "hub-a");
    let _: () = redis::cmd("SET")
        .arg(&local_key)
        .arg(serde_json::to_string(&local).unwrap())
        .arg("EX")
        .arg(300)
        .query(&mut conn)
        .unwrap();
    let older_remote = presence("local-agent", &ts(30), 300, "cloud");
    assert!(!apply_synced_presence(&mut conn, &settings, &older_remote, "hub-a", now).unwrap());
    let stored: String = redis::cmd("GET").arg(&local_key).query(&mut conn).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored).unwrap(),
        serde_json::to_value(&local).unwrap(),
        "the newer local row must be left untouched"
    );

    // An echo of this hub's own presence is dropped.
    let echo = presence("echo", &ts(1), 300, "hub-a");
    assert!(!apply_synced_presence(&mut conn, &settings, &echo, "hub-a", now).unwrap());

    let _: () = redis::cmd("DEL")
        .arg(&key)
        .arg(&local_key)
        .arg(format!("{}echo", settings.presence_prefix))
        .query(&mut conn)
        .unwrap();
}

#[ignore = "backend test: needs AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn schema_migration_is_additive_and_the_unique_index_is_partial() {
    let _ = PG_IGNORE;
    let tag = unique();
    let settings = pg_settings(&tag);
    let mut client = connect_postgres(&settings).unwrap().unwrap();

    // A table exactly as a pre-sync build created it: no sync columns.
    client
        .batch_execute(&format!(
            "create table {} (id uuid primary key, timestamp_utc timestamptz not null, \
              protocol_version text not null default '1.0', sender text not null, \
              recipient text not null, topic text not null, body text not null, \
              thread_id text null, priority text not null, tags jsonb not null default '[]'::jsonb, \
              request_ack boolean not null default false, reply_to text not null, \
              metadata jsonb not null default '{{}}'::jsonb, stream_id text null);",
            settings.message_table
        ))
        .unwrap();
    let legacy_id = uuid::Uuid::new_v4();
    client
        .execute(
            &format!(
                "insert into {} (id, timestamp_utc, sender, recipient, topic, body, priority, reply_to) \
                 values ($1, now(), 'old', 'all', 'status', 'legacy row', 'normal', '')",
                settings.message_table
            ),
            &[&legacy_id],
        )
        .unwrap();

    // The first write by a new build runs the (idempotent) migration.
    let a = message(&uuid::Uuid::new_v4().to_string(), Some("cloud"));
    persist_message_postgres(&settings, &a).unwrap();

    // Same (origin_hub, client_msg_id), different id: a silent no-op, not an error.
    let mut dup = message(&uuid::Uuid::new_v4().to_string(), Some("cloud"));
    dup.client_msg_id = a.client_msg_id.clone();
    persist_message_postgres(&settings, &dup).unwrap();

    // Same client_msg_id from another origin is a different message.
    let mut other_origin = message(&uuid::Uuid::new_v4().to_string(), Some("hub-b"));
    other_origin.client_msg_id = a.client_msg_id.clone();
    persist_message_postgres(&settings, &other_origin).unwrap();

    // NULL origin_hub never enters the index: two local rows may share a key.
    let mut local1 = message(&uuid::Uuid::new_v4().to_string(), None);
    local1.client_msg_id = Some("shared".to_owned());
    let mut local2 = message(&uuid::Uuid::new_v4().to_string(), None);
    local2.client_msg_id = Some("shared".to_owned());
    persist_message_postgres(&settings, &local1).unwrap();
    persist_message_postgres(&settings, &local2).unwrap();

    let count: i64 = client
        .query_one(
            &format!("select count(*) from {}", settings.message_table),
            &[],
        )
        .unwrap()
        .get(0);
    assert_eq!(
        count,
        1 + 1 + 1 + 2,
        "legacy + a + other origin + two local; dup dropped"
    );

    let rows =
        list_messages_postgres_with_filters(&settings, None, None, 60, 50, true, None, None, &[])
            .unwrap();
    let legacy = rows
        .iter()
        .find(|m| m.id == legacy_id.to_string())
        .expect("legacy row readable");
    assert!(
        legacy.origin_hub.is_none()
            && legacy.client_msg_id.is_none()
            && legacy.sensitivity.is_none(),
        "old rows read back with the new fields unset"
    );
    let stored = rows.iter().find(|m| m.id == a.id).unwrap();
    assert_eq!(stored.origin_hub.as_deref(), Some("cloud"));
    assert_eq!(stored.client_msg_id, a.client_msg_id);
    assert_eq!(stored.origin_seq, Some(7));
    assert_eq!(stored.hlc, a.hlc);
    assert_eq!(stored.sensitivity, Some(Sensitivity::Internal));

    drop_schema(&settings);
}

#[ignore = "backend test: needs AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
#[test]
fn presence_history_can_be_tailed_by_row_id() {
    let tag = unique();
    let settings = pg_settings(&tag);
    assert_eq!(presence_event_max_id(&settings).unwrap(), 0, "empty table");
    for n in 0..3 {
        let mut p = presence(&format!("agent-{n}"), &now_ts(), 60, "hub-a");
        p.metadata = serde_json::json!({});
        persist_presence_postgres(&settings, &p).unwrap();
    }
    let max = presence_event_max_id(&settings).unwrap();
    let first_two = list_presence_events_after(&settings, 0, 2).unwrap();
    assert_eq!(first_two.len(), 2);
    assert!(first_two[0].0 < first_two[1].0, "oldest first");
    let rest = list_presence_events_after(&settings, first_two[1].0, 10).unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].0, max);
    assert!(
        list_presence_events_after(&settings, max, 10)
            .unwrap()
            .is_empty()
    );

    drop_schema(&settings);
}
