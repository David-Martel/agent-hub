//! Repair of Redis/`PostgreSQL` divergence (agent-hub#194).
//!
//! [`crate::store_consistency`] finds ids that exist in one store only. This
//! module repairs the direction that can be repaired safely:
//!
//! - **Redis-only -> `PostgreSQL`**: the message is decoded (LZ4 bodies to
//!   plaintext) and inserted with `ON CONFLICT DO NOTHING`, so a repeat run is a
//!   no-op.
//! - **`PostgreSQL`-only -> Redis**: reported, never written. A stream accepts only
//!   increasing ids, so an old message could only be re-added at the tail, where
//!   `read` and every stream cursor would treat it as new. `PostgreSQL` is the
//!   durable archive and the right place to read such a message from.
//!
//! The default is a dry run. `apply` writes, and refuses to write more than
//! `max_insert` rows in one run so a wrong window cannot flood the archive.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use crate::error::{AgentBusError, Result};
use crate::models::Message;
use crate::postgres_store::{parse_timestamp_utc, sync_redis_to_postgres};
use crate::redis_bus::{connect, decode_stream_entry};
use crate::settings::Settings;
use crate::store_consistency::{Scope, StoreCheck, scan_stores};

/// Default cap on rows one `apply` may insert.
pub const DEFAULT_MAX_INSERT: usize = 1000;

const XRANGE_PAGE: usize = 1000;
const STREAM_START_SLACK_MS: i64 = 60_000;

/// Outcome of a reconcile run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReconcileReport {
    /// `true` when rows were written; `false` for the default dry run.
    pub applied: bool,
    /// Redis-only ids that a run with `apply` would insert into `PostgreSQL`.
    pub planned_redis_to_pg: usize,
    /// Rows actually inserted (always 0 for a dry run).
    pub inserted: usize,
    /// `PostgreSQL`-only ids. Reported, not repairable (see module docs).
    pub pg_only_not_restorable: usize,
    /// Comparison before any write.
    pub before: StoreCheck,
    /// Comparison after `apply`; absent for a dry run.
    pub after: Option<StoreCheck>,
}

/// Ids present in Redis and absent from `PostgreSQL`, sorted.
///
/// # Examples
///
/// ```
/// use agent_bus_core::reconcile::plan_redis_to_pg;
///
/// let redis = ["a".to_owned(), "b".to_owned()];
/// let pg = ["a".to_owned(), "c".to_owned()];
/// assert_eq!(plan_redis_to_pg(&redis, &pg), ["b"]);
/// ```
#[must_use]
pub fn plan_redis_to_pg(redis: &[String], pg: &[String]) -> Vec<String> {
    let pg: HashSet<&str> = pg.iter().map(String::as_str).collect();
    redis
        .iter()
        .filter(|id| !pg.contains(id.as_str()))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Decode the Redis entries whose id is in `wanted` and whose timestamp lies in `[from, to]`.
fn fetch_redis_messages(
    settings: &Settings,
    wanted: &HashSet<&str>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Message>> {
    let mut conn = connect(settings)?;
    let mut cursor = format!(
        "{}-0",
        (from.timestamp_millis() - STREAM_START_SLACK_MS).max(0)
    );
    let mut found: Vec<Message> = Vec::new();
    loop {
        let raw: Vec<redis::Value> = redis::cmd("XRANGE")
            .arg(&settings.stream_key)
            .arg(&cursor)
            .arg("+")
            .arg("COUNT")
            .arg(XRANGE_PAGE)
            .query(&mut conn)
            .map_err(|e| AgentBusError::Internal(format!("XRANGE failed: {e}")))?;
        let mut last: Option<String> = None;
        let count = raw.len();
        for entry in raw {
            let redis::Value::Array(parts) = entry else {
                continue;
            };
            let mut parts = parts.into_iter();
            let (Some(sid), Some(redis::Value::Array(flat))) = (parts.next(), parts.next()) else {
                continue;
            };
            let stream_id = match sid {
                redis::Value::BulkString(b) => String::from_utf8_lossy(&b).into_owned(),
                redis::Value::SimpleString(s) => s,
                _ => continue,
            };
            last = Some(stream_id.clone());
            let mut fields: HashMap<String, redis::Value> = HashMap::new();
            let mut flat = flat.into_iter();
            while let (Some(redis::Value::BulkString(key)), Some(value)) =
                (flat.next(), flat.next())
            {
                fields.insert(String::from_utf8_lossy(&key).into_owned(), value);
            }
            let mut message = decode_stream_entry(&fields);
            if !wanted.contains(message.id.as_str()) {
                continue;
            }
            let in_range =
                parse_timestamp_utc(&message.timestamp_utc).is_ok_and(|t| t >= from && t <= to);
            if in_range {
                message.stream_id = Some(stream_id);
                found.push(message);
            }
        }
        match last {
            Some(last) if count >= XRANGE_PAGE => cursor = format!("({last}"),
            _ => break,
        }
    }
    Ok(found)
}

/// Compare the stores and, with `apply`, backfill Redis-only messages into `PostgreSQL`.
///
/// A dry run (`apply == false`) performs no write.
///
/// # Errors
///
/// Returns an error when either store cannot be read or written, or when
/// `apply` would insert more than `max_insert` rows (nothing is written then).
pub fn reconcile(
    settings: &Settings,
    scope: Scope,
    settle: Duration,
    max_listed: usize,
    apply: bool,
    max_insert: usize,
) -> Result<ReconcileReport> {
    let scan = scan_stores(settings, scope, settle)?;
    let before = scan.check(max_listed);
    let plan = plan_redis_to_pg(&scan.redis_ids, &scan.pg_ids);
    let pg_only_not_restorable = before.report.pg_only_count;

    if !apply {
        return Ok(ReconcileReport {
            applied: false,
            planned_redis_to_pg: plan.len(),
            inserted: 0,
            pg_only_not_restorable,
            before,
            after: None,
        });
    }
    if plan.len() > max_insert {
        return Err(AgentBusError::Internal(format!(
            "refusing to backfill {} messages: more than --max-insert {max_insert}; \
             narrow the window or raise the cap after reviewing the dry run",
            plan.len()
        )));
    }

    let mut inserted = 0;
    if !plan.is_empty() {
        let wanted: HashSet<&str> = plan.iter().map(String::as_str).collect();
        let messages = fetch_redis_messages(settings, &wanted, scan.from, scan.to)?;
        let (_, newly) = sync_redis_to_postgres(settings, &messages)?;
        inserted = newly;
    }
    let after = scan_stores(settings, scope, settle)?.check(max_listed);
    Ok(ReconcileReport {
        applied: true,
        planned_redis_to_pg: plan.len(),
        inserted,
        pg_only_not_restorable,
        before,
        after: Some(after),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Protects: the plan is exactly Redis minus `PostgreSQL`, de-duplicated and sorted.
    #[test]
    fn plan_is_redis_minus_pg_sorted_and_unique() {
        let redis = ["c", "a", "b", "a", "d"].map(str::to_owned);
        let pg = ["b", "x"].map(str::to_owned);
        assert_eq!(plan_redis_to_pg(&redis, &pg), ["a", "c", "d"]);
    }

    /// Protects: `PostgreSQL`-only ids never enter the plan (they are not restorable).
    #[test]
    fn pg_only_ids_are_not_planned() {
        let redis = ["a"].map(str::to_owned);
        let pg = ["a", "only-in-pg"].map(str::to_owned);
        assert_eq!(plan_redis_to_pg(&redis, &pg), Vec::<String>::new());
    }

    /// Protects: reconcile refuses to run without `PostgreSQL`, even as a dry run.
    #[test]
    fn reconcile_without_postgres_is_an_error() {
        let mut settings = Settings::for_test();
        settings.database_url = None;
        assert!(reconcile(&settings, Scope::Full, Duration::seconds(0), 5, false, 10).is_err());
    }

    /// Dry run writes nothing, `apply` repairs the Redis-only side, a second
    /// `apply` is a no-op, and the `max_insert` guard refuses before writing.
    /// Detects: a dry run that writes, a non-idempotent backfill, a missing cap.
    #[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
    #[test]
    fn dry_run_then_apply_repairs_redis_only_and_is_idempotent() {
        use crate::postgres_store::persist_message_postgres;
        use uuid::Uuid;

        let mut settings = crate::test_support::backend_settings();
        let tag = Uuid::new_v4().simple().to_string();
        settings.stream_key = format!("agent_bus:test:reconcile:{tag}");
        settings.message_table = format!("test_reconcile_{tag}");
        let mut conn = connect(&settings).expect("Redis unreachable via AGENT_BUS_TEST_REDIS_URL");

        let stamp = (Utc::now() - Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string();
        let shared = Uuid::now_v7();
        let redis_only = Uuid::now_v7();
        let pg_only = Uuid::now_v7();
        let msg = |id: Uuid, body: &str| Message {
            id: id.to_string(),
            timestamp_utc: stamp.clone(),
            protocol_version: "1.0".to_owned(),
            from: "reconcile-test".to_owned(),
            to: "all".to_owned(),
            topic: "status".to_owned(),
            body: body.to_owned(),
            thread_id: None,
            tags: vec![].into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
            client_msg_id: None,
            origin_hub: None,
            origin_seq: None,
            hlc: None,
            sensitivity: None,
        };
        let xadd = |conn: &mut redis::Connection, id: Uuid, body: &str| {
            let _: String = redis::cmd("XADD")
                .arg(&settings.stream_key)
                .arg("*")
                .arg("id")
                .arg(id.to_string())
                .arg("timestamp_utc")
                .arg(&stamp)
                .arg("from")
                .arg("reconcile-test")
                .arg("to")
                .arg("all")
                .arg("topic")
                .arg("status")
                .arg("body")
                .arg(body)
                .query(conn)
                .expect("XADD");
        };
        xadd(&mut conn, shared, "both");
        persist_message_postgres(&settings, &msg(shared, "both")).expect("PG insert");
        xadd(&mut conn, redis_only, "redis side");
        persist_message_postgres(&settings, &msg(pg_only, "pg side")).expect("PG insert");

        let none = Duration::seconds(0);
        let dry = reconcile(&settings, Scope::Full, none, 10, false, 10).expect("dry run");
        assert!(!dry.applied);
        assert_eq!((dry.planned_redis_to_pg, dry.inserted), (1, 0));
        assert!(!dry.before.report.ok, "a dry run must leave the divergence");

        let refused = reconcile(&settings, Scope::Full, none, 10, true, 0);
        assert!(refused.is_err(), "max_insert must refuse before writing");
        let still = scan_stores(&settings, Scope::Full, none).expect("scan");
        assert_eq!(
            still.check(10).report.redis_only_count,
            1,
            "refusal wrote nothing"
        );

        let applied = reconcile(&settings, Scope::Full, none, 10, true, 10).expect("apply");
        assert_eq!(applied.inserted, 1);
        let after = applied.after.expect("apply reports the after state");
        assert_eq!(after.report.redis_only_count, 0, "Redis-only side repaired");
        assert_eq!(
            after.report.pg_only,
            [pg_only.to_string()],
            "PG-only is reported, not written"
        );
        assert_eq!(applied.pg_only_not_restorable, 1);

        let again = reconcile(&settings, Scope::Full, none, 10, true, 10).expect("re-apply");
        assert_eq!(
            (again.planned_redis_to_pg, again.inserted),
            (0, 0),
            "idempotent"
        );
    }
}
