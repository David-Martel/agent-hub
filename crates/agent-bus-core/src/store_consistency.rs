//! Cross-store message-ID consistency between the Redis stream and `PostgreSQL`.
//!
//! `health` used to compare only `pg_message_count` with `stream_length`. Two
//! stores can hold the same number of messages and still disagree on which
//! ones (agent-hub#194: 32 `PostgreSQL`-only and 32 Redis-only ids at equal
//! counts). This module compares the *identity* sets instead.
//!
//! - [`id_set_digest`] is an order-independent digest, cheap to log and diff.
//! - [`compare_id_sets`] is the pure comparison and lists differing ids,
//!   capped, so a mismatch is actionable.
//! - [`check_stores`] reads both stores for a bounded time window or in full.
//!
//! The window is on each message's own `timestamp_utc`, ends `settle` seconds
//! in the past so the write-behind queue (flushed every 100 ms) is not reported
//! as divergence, and starts no earlier than the oldest entry Redis still
//! holds, because `MAXLEN` trimming legitimately leaves older rows only in
//! `PostgreSQL`.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::{AgentBusError, Result};
use crate::postgres_store::{list_message_ids_between, parse_timestamp_utc};
use crate::redis_bus::connect;
use crate::settings::Settings;

/// Default distance from "now" before a message is judged, in seconds.
pub const DEFAULT_SETTLE_SECONDS: i64 = 30;

/// Default cap on ids listed per side of a mismatch.
pub const DEFAULT_MAX_LISTED: usize = 50;

/// Entries fetched per `XRANGE` page.
const XRANGE_PAGE: usize = 1000;

/// Slack when converting a message timestamp to a stream-id start, in ms.
///
/// The stream id is assigned by Redis at `XADD`, the message timestamp by the
/// sender a little earlier; the slack keeps the boundary entry in range.
const STREAM_START_SLACK_MS: i64 = 60_000;

/// How much history to compare.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Messages whose `timestamp_utc` falls in the last `Duration`.
    Window(Duration),
    /// Everything Redis still retains.
    Full,
}

/// Result of comparing the Redis and `PostgreSQL` id sets.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdSetReport {
    /// `true` when both stores hold exactly the same ids.
    pub ok: bool,
    /// Ids compared on the Redis side.
    pub redis_count: usize,
    /// Ids compared on the `PostgreSQL` side.
    pub pg_count: usize,
    /// Order-independent digest of the Redis ids.
    pub redis_digest: String,
    /// Order-independent digest of the `PostgreSQL` ids.
    pub pg_digest: String,
    /// Number of ids present only in Redis.
    pub redis_only_count: usize,
    /// Number of ids present only in `PostgreSQL`.
    pub pg_only_count: usize,
    /// Up to `max_listed` Redis-only ids, sorted.
    pub redis_only: Vec<String>,
    /// Up to `max_listed` `PostgreSQL`-only ids, sorted.
    pub pg_only: Vec<String>,
    /// `true` when either list was cut at `max_listed`.
    pub listing_truncated: bool,
}

/// Window and bookkeeping around an [`IdSetReport`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoreCheck {
    /// `window` or `full`.
    pub scope: String,
    /// Inclusive start of the compared range.
    pub from_utc: String,
    /// Inclusive end of the compared range.
    pub to_utc: String,
    /// Oldest message timestamp Redis still holds, if any.
    pub redis_retention_start_utc: Option<String>,
    /// Redis entries whose `id` is not a UUID (never persisted to `PostgreSQL`).
    pub redis_non_uuid_skipped: usize,
    /// Redis entries with a missing or unparseable `timestamp_utc`.
    pub redis_untimed_skipped: usize,
    /// The id-set comparison.
    #[serde(flatten)]
    pub report: IdSetReport,
}

/// Order-independent digest of a set of message ids.
///
/// Each id is hashed with SHA-256, the first 128 bits are summed with
/// wrapping addition, and the sum is rendered as 32 hex digits. The digest does
/// not depend on iteration order and two different sets collide only with
/// cryptographic-hash-sum probability. Duplicates are collapsed first so a
/// repeated id cannot mask a missing one.
///
/// # Examples
///
/// ```
/// use agent_bus_core::store_consistency::id_set_digest;
///
/// let a = id_set_digest(["x", "y", "z"]);
/// let b = id_set_digest(["z", "x", "y", "x"]);
/// assert_eq!(a, b);
/// assert_ne!(a, id_set_digest(["x", "y"]));
/// ```
#[must_use]
pub fn id_set_digest<I, S>(ids: I) -> String
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let unique: BTreeSet<String> = ids.into_iter().map(|id| id.as_ref().to_owned()).collect();
    let mut sum: u128 = 0;
    for id in &unique {
        let hash = Sha256::digest(id.as_bytes());
        let mut head = [0_u8; 16];
        head.copy_from_slice(&hash[..16]);
        sum = sum.wrapping_add(u128::from_be_bytes(head));
    }
    format!("{sum:032x}")
}

/// Compare two id sets and list the differences, capped at `max_listed` per side.
///
/// # Examples
///
/// ```
/// use agent_bus_core::store_consistency::compare_id_sets;
///
/// let redis = ["a".to_owned(), "b".to_owned()];
/// let pg = ["a".to_owned(), "c".to_owned()];
/// let report = compare_id_sets(&redis, &pg, 10);
/// assert!(!report.ok);
/// assert_eq!(report.redis_only, ["b"]);
/// assert_eq!(report.pg_only, ["c"]);
/// ```
#[must_use]
pub fn compare_id_sets(redis: &[String], pg: &[String], max_listed: usize) -> IdSetReport {
    let redis_set: BTreeSet<&str> = redis.iter().map(String::as_str).collect();
    let pg_set: BTreeSet<&str> = pg.iter().map(String::as_str).collect();
    let redis_only: Vec<&str> = redis_set.difference(&pg_set).copied().collect();
    let pg_only: Vec<&str> = pg_set.difference(&redis_set).copied().collect();
    IdSetReport {
        ok: redis_only.is_empty() && pg_only.is_empty(),
        redis_count: redis_set.len(),
        pg_count: pg_set.len(),
        redis_digest: id_set_digest(redis_set.iter()),
        pg_digest: id_set_digest(pg_set.iter()),
        redis_only_count: redis_only.len(),
        pg_only_count: pg_only.len(),
        listing_truncated: redis_only.len() > max_listed || pg_only.len() > max_listed,
        redis_only: redis_only
            .into_iter()
            .take(max_listed)
            .map(str::to_owned)
            .collect(),
        pg_only: pg_only
            .into_iter()
            .take(max_listed)
            .map(str::to_owned)
            .collect(),
    }
}

fn field_text(fields: &[redis::Value], name: &str) -> Option<String> {
    fields.as_chunks::<2>().0.iter().find_map(|pair| {
        let key = value_text(&pair[0])?;
        (key == name).then(|| value_text(&pair[1])).flatten()
    })
}

fn value_text(value: &redis::Value) -> Option<String> {
    match value {
        redis::Value::BulkString(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        redis::Value::SimpleString(text) => Some(text.clone()),
        _ => None,
    }
}

/// One parsed `XRANGE` page: `(stream_id, fields)` pairs.
fn parse_range_page(raw: Vec<redis::Value>) -> Vec<(String, Vec<redis::Value>)> {
    raw.into_iter()
        .filter_map(|entry| {
            let redis::Value::Array(parts) = entry else {
                return None;
            };
            let mut parts = parts.into_iter();
            let stream_id = value_text(&parts.next()?)?;
            let redis::Value::Array(fields) = parts.next()? else {
                return None;
            };
            Some((stream_id, fields))
        })
        .collect()
}

/// Redis ids in `[from, to]` plus skip counters and the retention start.
struct RedisIds {
    ids: Vec<String>,
    non_uuid_skipped: usize,
    untimed_skipped: usize,
    retention_start: Option<DateTime<Utc>>,
    /// Lower bound actually applied to the Redis ids; PostgreSQL must use it too.
    effective_from: DateTime<Utc>,
}

fn read_redis_ids(settings: &Settings, from: DateTime<Utc>, to: DateTime<Utc>) -> Result<RedisIds> {
    let mut conn = connect(settings)?;
    let xrange_err = |e: redis::RedisError| AgentBusError::Internal(format!("XRANGE failed: {e}"));

    let first: Vec<redis::Value> = redis::cmd("XRANGE")
        .arg(&settings.stream_key)
        .arg("-")
        .arg("+")
        .arg("COUNT")
        .arg(1)
        .query(&mut conn)
        .map_err(xrange_err)?;
    // The boundary is the first retained entry's `timestamp_utc`, so that entry
    // itself is inside it. An entry without a usable timestamp falls back to
    // its stream-id time rather than hiding the boundary.
    let retention_start =
        parse_range_page(first)
            .into_iter()
            .next()
            .and_then(|(stream_id, fields)| {
                field_text(&fields, "timestamp_utc")
                    .and_then(|ts| parse_timestamp_utc(&ts).ok())
                    .or_else(|| {
                        let ms = stream_id.split('-').next()?.parse::<i64>().ok()?;
                        DateTime::<Utc>::from_timestamp_millis(ms)
                    })
            });
    // One lower bound for both stores, applied to Redis entries below and to
    // the PostgreSQL query by the caller.
    let from = retention_start.map_or(from, |start| from.max(start));

    let start_ms = (from.timestamp_millis() - STREAM_START_SLACK_MS).max(0);
    let mut cursor = format!("{start_ms}-0");
    let mut out = RedisIds {
        ids: Vec::new(),
        non_uuid_skipped: 0,
        untimed_skipped: 0,
        retention_start,
        effective_from: from,
    };
    loop {
        let raw: Vec<redis::Value> = redis::cmd("XRANGE")
            .arg(&settings.stream_key)
            .arg(&cursor)
            .arg("+")
            .arg("COUNT")
            .arg(XRANGE_PAGE)
            .query(&mut conn)
            .map_err(xrange_err)?;
        let page = parse_range_page(raw);
        let Some(last) = page.last().map(|(id, _)| id.clone()) else {
            break;
        };
        let full_page = page.len() >= XRANGE_PAGE;
        for (_, fields) in &page {
            let Some(timestamp) =
                field_text(fields, "timestamp_utc").and_then(|ts| parse_timestamp_utc(&ts).ok())
            else {
                out.untimed_skipped += 1;
                continue;
            };
            if timestamp < from || timestamp > to {
                continue;
            }
            match field_text(fields, "id").and_then(|id| Uuid::parse_str(&id).ok()) {
                Some(uuid) => out.ids.push(uuid.to_string()),
                None => out.non_uuid_skipped += 1,
            }
        }
        if !full_page {
            break;
        }
        cursor = format!("({last}");
    }
    Ok(out)
}

/// Both stores' ids for one window, uncapped, before comparison.
#[derive(Debug)]
pub struct StoreScan {
    /// `window` or `full`.
    pub scope: &'static str,
    /// Inclusive start of the compared range (clamped to Redis retention).
    pub from: DateTime<Utc>,
    /// Inclusive end of the compared range.
    pub to: DateTime<Utc>,
    /// Oldest message timestamp Redis still holds, if any.
    pub redis_retention_start: Option<DateTime<Utc>>,
    /// Redis ids in range.
    pub redis_ids: Vec<String>,
    /// `PostgreSQL` ids in range.
    pub pg_ids: Vec<String>,
    /// Redis entries whose `id` is not a UUID.
    pub redis_non_uuid_skipped: usize,
    /// Redis entries with a missing or unparseable `timestamp_utc`.
    pub redis_untimed_skipped: usize,
}

/// Read both stores' ids for `scope`, judging messages up to `now - settle`.
///
/// # Errors
///
/// Returns an error when `PostgreSQL` is not configured, or when either store
/// cannot be read.
pub fn scan_stores(settings: &Settings, scope: Scope, settle: Duration) -> Result<StoreScan> {
    if settings.database_url.is_none() {
        return Err(AgentBusError::Internal(
            "store consistency needs PostgreSQL (database_url is not configured)".to_owned(),
        ));
    }
    let to = Utc::now() - settle;
    let requested_from = match scope {
        Scope::Window(window) => to - window,
        Scope::Full => DateTime::<Utc>::UNIX_EPOCH,
    };
    let redis = read_redis_ids(settings, requested_from, to)?;
    // Both stores are judged over the bound `read_redis_ids` applied.
    let from = redis.effective_from;
    let pg_ids = list_message_ids_between(settings, from, to)?;
    Ok(StoreScan {
        scope: match scope {
            Scope::Window(_) => "window",
            Scope::Full => "full",
        },
        from,
        to,
        redis_retention_start: redis.retention_start,
        redis_ids: redis.ids,
        pg_ids,
        redis_non_uuid_skipped: redis.non_uuid_skipped,
        redis_untimed_skipped: redis.untimed_skipped,
    })
}

impl StoreScan {
    /// Compare the two id sets, listing at most `max_listed` differing ids per side.
    #[must_use]
    pub fn check(&self, max_listed: usize) -> StoreCheck {
        StoreCheck {
            scope: self.scope.to_owned(),
            from_utc: self.from.to_rfc3339(),
            to_utc: self.to.to_rfc3339(),
            redis_retention_start_utc: self.redis_retention_start.map(|ts| ts.to_rfc3339()),
            redis_non_uuid_skipped: self.redis_non_uuid_skipped,
            redis_untimed_skipped: self.redis_untimed_skipped,
            report: compare_id_sets(&self.redis_ids, &self.pg_ids, max_listed),
        }
    }
}

/// Compare the Redis stream and `PostgreSQL` for `scope`, judging messages up to
/// `now - settle`.
///
/// # Errors
///
/// Returns an error when `PostgreSQL` is not configured, or when either store
/// cannot be read.
pub fn check_stores(
    settings: &Settings,
    scope: Scope,
    settle: Duration,
    max_listed: usize,
) -> Result<StoreCheck> {
    Ok(scan_stores(settings, scope, settle)?.check(max_listed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    /// Protects: the digest ignores order and duplicates.
    #[test]
    fn digest_is_order_independent_and_set_valued() {
        assert_eq!(
            id_set_digest(["a", "b", "c"]),
            id_set_digest(["c", "a", "b", "a"])
        );
    }

    /// Protects: any single-id difference changes the digest.
    /// Detects: a digest that only looks at counts or a prefix.
    #[test]
    fn digest_changes_when_one_id_is_swapped() {
        assert_ne!(
            id_set_digest(["a", "b", "c"]),
            id_set_digest(["a", "b", "d"])
        );
        assert_ne!(id_set_digest(["a", "b", "c"]), id_set_digest(["a", "b"]));
    }

    /// Positive control for the #194 shape: equal counts, different identities.
    /// This is the case a count comparison reports as healthy.
    #[test]
    fn equal_counts_with_different_ids_fail_and_name_the_ids() {
        let redis = ids(&["m1", "m2", "m3", "r-only"]);
        let pg = ids(&["m1", "m2", "m3", "p-only"]);
        assert_eq!(redis.len(), pg.len(), "precondition: counts agree");
        let report = compare_id_sets(&redis, &pg, 10);
        assert!(!report.ok, "a one-sided write on each store must fail");
        assert_eq!(report.redis_only, ids(&["r-only"]));
        assert_eq!(report.pg_only, ids(&["p-only"]));
        assert_ne!(report.redis_digest, report.pg_digest);
    }

    /// Protects: identical sets pass, so a failure above is not a constant `false`.
    #[test]
    fn identical_sets_pass_regardless_of_order() {
        let report = compare_id_sets(&ids(&["a", "b", "c"]), &ids(&["c", "b", "a"]), 10);
        assert!(report.ok);
        assert_eq!(report.redis_digest, report.pg_digest);
        assert!(report.redis_only.is_empty() && report.pg_only.is_empty());
    }

    /// Protects: the listing is capped but the counts stay exact.
    #[test]
    fn listing_is_capped_and_flagged() {
        let many: Vec<String> = (0..120).map(|n| format!("id-{n:03}")).collect();
        let report = compare_id_sets(&many, &[], 50);
        assert_eq!(report.redis_only_count, 120);
        assert_eq!(report.redis_only.len(), 50);
        assert!(report.listing_truncated);
        assert_eq!(report.redis_only[0], "id-000", "listing is sorted");
    }

    /// Protects: a stream page parses into ids and fields.
    #[test]
    fn range_page_parses_entries() {
        let entry = redis::Value::Array(vec![
            redis::Value::BulkString(b"1-0".to_vec()),
            redis::Value::Array(vec![
                redis::Value::BulkString(b"id".to_vec()),
                redis::Value::BulkString(b"abc".to_vec()),
            ]),
        ]);
        let page = parse_range_page(vec![entry]);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].0, "1-0");
        assert_eq!(field_text(&page[0].1, "id").as_deref(), Some("abc"));
        assert_eq!(field_text(&page[0].1, "missing"), None);
    }

    /// Protects: `check_stores` refuses to report "ok" with no `PostgreSQL`.
    #[test]
    fn check_without_postgres_is_an_error_not_a_pass() {
        let mut settings = Settings::for_test();
        settings.database_url = None;
        let err = check_stores(&settings, Scope::Full, Duration::seconds(0), 5)
            .expect_err("no PostgreSQL must be an error");
        assert!(err.to_string().contains("PostgreSQL"), "got: {err}");
    }

    /// Positive control against real stores: a write to only one side must
    /// fail the check, and repairing both sides must make it pass.
    /// Detects: a check that cannot fail (agent-hub#194).
    #[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
    #[test]
    fn one_sided_write_fails_then_repair_passes() {
        use crate::models::Message;
        use crate::postgres_store::persist_message_postgres;

        let mut settings = crate::test_support::backend_settings();
        let tag = Uuid::new_v4().simple().to_string();
        settings.stream_key = format!("agent_bus:test:consistency:{tag}");
        settings.message_table = format!("test_consistency_{tag}");
        let mut conn = connect(&settings).expect("Redis unreachable via AGENT_BUS_TEST_REDIS_URL");

        let stamp = (Utc::now() - Duration::minutes(5))
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string();
        let message = |id: Uuid| Message {
            id: id.to_string(),
            timestamp_utc: stamp.clone(),
            protocol_version: "1.0".to_owned(),
            from: "consistency-test".to_owned(),
            to: "all".to_owned(),
            topic: "status".to_owned(),
            body: "x".to_owned(),
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
        let xadd = |conn: &mut redis::Connection, id: Uuid| {
            let _: String = redis::cmd("XADD")
                .arg(&settings.stream_key)
                .arg("*")
                .arg("id")
                .arg(id.to_string())
                .arg("timestamp_utc")
                .arg(&stamp)
                .query(conn)
                .expect("XADD");
        };

        let (both, redis_only, pg_only) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        xadd(&mut conn, both);
        persist_message_postgres(&settings, &message(both)).expect("PG insert");
        // Equal counts (2 and 2) but different identities.
        xadd(&mut conn, redis_only);
        persist_message_postgres(&settings, &message(pg_only)).expect("PG insert");

        let check =
            check_stores(&settings, Scope::Full, Duration::seconds(0), 10).expect("check must run");
        assert_eq!(check.report.redis_count, check.report.pg_count);
        assert!(!check.report.ok, "one-sided writes must fail the check");
        assert_eq!(check.report.redis_only, [redis_only.to_string()]);
        assert_eq!(check.report.pg_only, [pg_only.to_string()]);

        xadd(&mut conn, pg_only);
        persist_message_postgres(&settings, &message(redis_only)).expect("PG insert");
        let check =
            check_stores(&settings, Scope::Full, Duration::seconds(0), 10).expect("check must run");
        assert!(check.report.ok, "repaired stores must pass: {check:?}");
        assert_eq!(check.report.redis_digest, check.report.pg_digest);
    }

    /// Protects: both stores are judged over the same lower bound even when a
    /// synced entry keeps an old `timestamp_utc` but lands late in the stream
    /// (Codex and gemini review of agent-hub#197).
    /// Detects: identical stores reported as divergent when the first retained
    /// entry is newer than a later synced one.
    #[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL + AGENT_BUS_TEST_DATABASE_URL (see tests/support/backend_env.rs)"]
    #[test]
    fn synced_entry_older_than_first_retained_entry_is_not_divergence() {
        use crate::models::Message;
        use crate::postgres_store::persist_message_postgres;

        let mut settings = crate::test_support::backend_settings();
        let tag = Uuid::new_v4().simple().to_string();
        settings.stream_key = format!("agent_bus:test:consistency:{tag}");
        settings.message_table = format!("test_consistency_{tag}");
        let mut conn = connect(&settings).expect("Redis unreachable via AGENT_BUS_TEST_REDIS_URL");

        let fmt = "%Y-%m-%dT%H:%M:%S%.6fZ";
        let recent = (Utc::now() - Duration::minutes(5)).format(fmt).to_string();
        let old = (Utc::now() - Duration::hours(2)).format(fmt).to_string();
        for (id, stamp) in [(Uuid::now_v7(), &recent), (Uuid::now_v7(), &old)] {
            let _: String = redis::cmd("XADD")
                .arg(&settings.stream_key)
                .arg("*")
                .arg("id")
                .arg(id.to_string())
                .arg("timestamp_utc")
                .arg(stamp)
                .query(&mut conn)
                .expect("XADD");
            persist_message_postgres(
                &settings,
                &Message {
                    id: id.to_string(),
                    timestamp_utc: stamp.clone(),
                    protocol_version: "1.0".to_owned(),
                    from: "consistency-test".to_owned(),
                    to: "all".to_owned(),
                    topic: "status".to_owned(),
                    body: "x".to_owned(),
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
                },
            )
            .expect("PG insert");
        }

        let check =
            check_stores(&settings, Scope::Full, Duration::seconds(0), 10).expect("check must run");
        assert!(check.report.ok, "identical stores must pass: {check:?}");
    }
}
