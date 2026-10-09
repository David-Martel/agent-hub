//! Local-store primitives for the hub-side cloud sync task (agent-hub#79).
//!
//! The sync task itself lives in `agent-bus-http`. These functions are the
//! Redis half of what it needs, kept here beside the stream encoding they
//! depend on:
//!
//! - [`read_messages_after`] tails the message stream from a cursor.
//! - [`ingest_synced_message`] writes a message pulled from the cloud into the
//!   local stream, idempotently, preserving its `origin_hub`.
//! - [`apply_synced_presence`] merges a pulled presence row without ever
//!   overriding a newer local one.

use redis::Commands;

use crate::error::{AgentBusError, Result};
use crate::models::{Message, Presence};
use crate::postgres_store::{PgWriter, parse_timestamp_utc};
use crate::redis_bus::{
    append_notifications_for_message, decode_stream_entry, parse_xrange_result,
};
use crate::settings::Settings;

/// How long an ingested `(origin_hub, id)` pair is remembered for
/// de-duplication. Replays older than this after a lost cursor are re-ingested.
pub const INGEST_SEEN_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

// Redis scripts are atomic, but do not roll back successful writes on errors.
// Reserve the marker and append in one invocation; explicitly remove the
// reservation when XADD fails, before another ingestion can observe it.
const INGEST_SCRIPT: &str = r"
if redis.call('EXISTS', KEYS[1]) == 1 then return false end
local stream_type = redis.call('TYPE', KEYS[2]).ok
if stream_type ~= 'none' and stream_type ~= 'stream' then
    return redis.error_reply('synced message stream has the wrong type')
end
if redis.acl_check_cmd then
    if not redis.acl_check_cmd('SET', KEYS[1], '1', 'EX', ARGV[1]) or
       not redis.acl_check_cmd('DEL', KEYS[1]) or
       not redis.acl_check_cmd('XADD', KEYS[2], 'MAXLEN', '~', ARGV[2], '*', unpack(ARGV, 3)) then
        return redis.error_reply('synced message ingestion requires SET, DEL and XADD permission')
    end
end
-- Redis 6.2 has no acl_check_cmd. Since the key is absent, this is a no-op
-- which verifies cleanup permission before reserving it on every version.
redis.call('DEL', KEYS[1])
redis.call('SET', KEYS[1], '1', 'EX', ARGV[1])
local added = redis.pcall('XADD', KEYS[2], 'MAXLEN', '~', ARGV[2], '*', unpack(ARGV, 3))
if type(added) == 'table' and added.err then
    redis.call('DEL', KEYS[1])
    return redis.error_reply(added.err)
end
return added
";

const PRESENCE_CAS_SCRIPT: &str = r"
local current = redis.call('GET', KEYS[1])
if ARGV[1] == 'absent' then
    if current then return 0 end
elseif current ~= ARGV[2] then
    return 0
end
redis.call('SET', KEYS[1], ARGV[3], 'EX', ARGV[4])
return 1
";

const PRESENCE_CAS_MAX_ATTEMPTS: usize = 8;

/// Redis key recording that `(origin_hub, id)` has already been ingested.
#[must_use]
pub fn ingest_seen_key(origin_hub: &str, id: &str) -> String {
    format!("agent_bus:cloud_sync:ingested:{origin_hub}:{id}")
}

/// Up to `count` stream entries strictly after `after` (`"0-0"` for the
/// start), oldest first, each with `stream_id` set.
///
/// Uses the exclusive `(id` range form, which needs Redis 6.2 or later, as the
/// rest of this crate already does.
///
/// # Errors
/// Returns an error if the `XRANGE` command fails.
pub fn read_messages_after(
    conn: &mut redis::Connection,
    settings: &Settings,
    after: &str,
    count: usize,
) -> Result<Vec<Message>> {
    let start = if after == "0-0" || after.is_empty() {
        "-".to_owned()
    } else {
        format!("({after}")
    };
    let raw: Vec<redis::Value> = redis::cmd("XRANGE")
        .arg(&settings.stream_key)
        .arg(&start)
        .arg("+")
        .arg("COUNT")
        .arg(count)
        .query(conn)
        .map_err(|e| AgentBusError::Internal(format!("XRANGE after cursor failed: {e}")))?;
    Ok(parse_xrange_result(&raw)
        .into_iter()
        .map(|(stream_id, fields)| {
            let mut msg = decode_stream_entry(&fields);
            msg.stream_id = Some(stream_id);
            msg
        })
        .collect())
}

/// Result of [`ingest_synced_message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestOutcome {
    /// The message was written to the local stream.
    Ingested,
    /// `(origin_hub, id)` was already ingested; nothing was written.
    Duplicate,
}

/// Write a message pulled from the cloud into the local stream.
///
/// The message must carry an `origin_hub`; it is stored in the stream entry so
/// the push tailer can see the message is not local and never send it back.
/// Ingest is idempotent on `(origin_hub, id)`. Ack tracking, ownership
/// tracking and pub/sub are deliberately skipped: this is replay of an event
/// that already happened elsewhere. Per-recipient notifications are still
/// appended so `check_inbox` sees the message.
///
/// # Errors
/// Returns an error if the message has no `origin_hub` or `id`, or if a Redis
/// command fails. The dedup marker and stream append are server-atomic; a
/// failed append leaves no marker, so a retry can succeed.
pub fn ingest_synced_message(
    conn: &mut redis::Connection,
    settings: &Settings,
    msg: &Message,
    pg_writer: Option<&PgWriter>,
) -> Result<IngestOutcome> {
    let origin_hub = msg
        .origin_hub
        .as_deref()
        .filter(|hub| !hub.is_empty())
        .ok_or_else(|| AgentBusError::InvalidParams("synced message has no origin_hub".into()))?;
    if msg.id.is_empty() {
        return Err(AgentBusError::InvalidParams(
            "synced message has no id".into(),
        ));
    }

    let Some(stream_id) = xadd_synced(conn, settings, msg, origin_hub)? else {
        return Ok(IngestOutcome::Duplicate);
    };
    let mut stored = msg.clone();
    stored.stream_id = Some(stream_id);
    if let Some(writer) = pg_writer {
        if postgres_can_store(&stored) {
            writer.send_message(&stored);
        } else {
            // A row `PostgreSQL` would reject must not reach the
            // writer: every rejected write is retried three times and
            // then opens the 60 s circuit breaker, which would stall
            // durable persistence of LOCAL messages. It stays
            // Redis-only.
            tracing::warn!(
                "synced message {} has an id or timestamp PostgreSQL cannot store; \
                         keeping it in Redis only",
                stored.id
            );
        }
    }
    if let Err(error) = append_notifications_for_message(conn, &stored) {
        tracing::warn!(
            "failed to append notification for synced {}: {error:#}",
            msg.id
        );
    }
    Ok(IngestOutcome::Ingested)
}

/// Whether the `PostgreSQL` row for `msg` would be accepted: the `id` column is
/// a `uuid` and `timestamp_utc` must parse. Historical or foreign ids need not
/// be UUIDs on the cloud side, so pulled messages are checked first.
#[must_use]
pub fn postgres_can_store(msg: &Message) -> bool {
    uuid::Uuid::parse_str(&msg.id).is_ok() && parse_timestamp_utc(&msg.timestamp_utc).is_ok()
}

fn xadd_synced(
    conn: &mut redis::Connection,
    settings: &Settings,
    msg: &Message,
    origin_hub: &str,
) -> Result<Option<String>> {
    let tags = serde_json::to_string(msg.tags.as_slice()).unwrap_or_else(|_| "[]".to_owned());
    let metadata = serde_json::to_string(&msg.metadata).unwrap_or_else(|_| "{}".to_owned());
    let mut fields: Vec<(&str, String)> = vec![
        ("id", msg.id.clone()),
        ("timestamp_utc", msg.timestamp_utc.clone()),
        ("protocol_version", msg.protocol_version.clone()),
        ("from", msg.from.clone()),
        ("to", msg.to.clone()),
        ("topic", msg.topic.clone()),
        ("body", msg.body.clone()),
        ("tags", tags),
        ("priority", msg.priority.clone()),
        ("request_ack", msg.request_ack.to_string()),
        ("reply_to", msg.reply_to.clone().unwrap_or_default()),
        ("metadata", metadata),
        ("origin_hub", origin_hub.to_owned()),
    ];
    let optional = [
        ("thread_id", msg.thread_id.clone()),
        ("client_msg_id", msg.client_msg_id.clone()),
        ("origin_seq", msg.origin_seq.map(|seq| seq.to_string())),
        ("hlc", msg.hlc.clone()),
        (
            "sensitivity",
            msg.sensitivity.map(|value| value.as_str().to_owned()),
        ),
    ];
    fields.extend(
        optional
            .into_iter()
            .filter_map(|(name, value)| value.map(|value| (name, value))),
    );
    redis::Script::new(INGEST_SCRIPT)
        .key(ingest_seen_key(origin_hub, &msg.id))
        .key(&settings.stream_key)
        .arg(INGEST_SEEN_TTL_SECONDS)
        .arg(settings.stream_maxlen)
        .arg(&fields)
        .invoke(conn)
        .map_err(|e| AgentBusError::Internal(format!("atomic synced ingest failed: {e}")))
}

/// Decide whether a pulled presence row should be written, and for how long.
///
/// Returns the TTL in seconds to store it with, or `None` to drop it. A row is
/// dropped when it originated at `local_hub` (an echo of this hub's own
/// announcement), when it has already expired at `now`, or when the local row
/// for the same agent is as new or newer: remote state must never mask a live
/// local agent.
///
/// # Errors
/// Returns an error if the incoming timestamp is not valid RFC 3339.
pub fn synced_presence_ttl(
    incoming: &Presence,
    local: Option<&Presence>,
    local_hub: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<u64>> {
    let origin = incoming
        .metadata
        .get("origin_hub")
        .and_then(serde_json::Value::as_str);
    if origin == Some(local_hub) {
        return Ok(None);
    }
    let incoming_ts = parse_timestamp_utc(&incoming.timestamp_utc)?;
    let age = now.signed_duration_since(incoming_ts).num_seconds().max(0);
    let remaining = i64::try_from(incoming.ttl_seconds)
        .unwrap_or(i64::MAX)
        .saturating_sub(age);
    if remaining <= 0 {
        return Ok(None);
    }
    if let Some(local) = local
        && let Ok(local_ts) = parse_timestamp_utc(&local.timestamp_utc)
        && local_ts >= incoming_ts
    {
        return Ok(None);
    }
    Ok(Some(u64::try_from(remaining).unwrap_or(1)))
}

/// Merge a presence row pulled from the cloud into the local presence cache.
///
/// Returns `true` if a row was written; see [`synced_presence_ttl`] for when it
/// is not. Written rows are Redis-only: they are never added to the durable
/// presence history the push side tails, so they cannot be pushed back.
///
/// # Errors
/// Returns an error if a Redis command fails or the row's timestamp is invalid.
pub fn apply_synced_presence(
    conn: &mut redis::Connection,
    settings: &Settings,
    presence: &Presence,
    local_hub: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    apply_synced_presence_with_cas(
        conn,
        settings,
        presence,
        local_hub,
        now,
        compare_and_set_presence,
    )
}

fn apply_synced_presence_with_cas(
    conn: &mut redis::Connection,
    settings: &Settings,
    presence: &Presence,
    local_hub: &str,
    now: chrono::DateTime<chrono::Utc>,
    mut compare_and_set: impl FnMut(
        &mut redis::Connection,
        &str,
        Option<&str>,
        &str,
        u64,
    ) -> Result<bool>,
) -> Result<bool> {
    let key = format!("{}{}", settings.presence_prefix, presence.agent);
    let json = serde_json::to_string(presence)
        .map_err(|e| AgentBusError::Internal(format!("serialize presence: {e}")))?;
    for _ in 0..PRESENCE_CAS_MAX_ATTEMPTS {
        let existing: Option<String> = conn
            .get(&key)
            .map_err(|e| AgentBusError::Internal(format!("GET presence failed: {e}")))?;
        let local = existing
            .as_deref()
            .and_then(|json| serde_json::from_str::<Presence>(json).ok());
        let Some(ttl) = synced_presence_ttl(presence, local.as_ref(), local_hub, now)? else {
            return Ok(false);
        };
        if compare_and_set(conn, &key, existing.as_deref(), &json, ttl)? {
            return Ok(true);
        }
        // A local announcement changed the snapshot. Compare its timestamp
        // before attempting another write; never overwrite it using stale data.
    }
    Err(AgentBusError::Internal(
        "presence changed during every sync update attempt; retry on the next pull".into(),
    ))
}

fn compare_and_set_presence(
    conn: &mut redis::Connection,
    key: &str,
    expected: Option<&str>,
    json: &str,
    ttl: u64,
) -> Result<bool> {
    redis::Script::new(PRESENCE_CAS_SCRIPT)
        .key(key)
        .arg(if expected.is_some() {
            "present"
        } else {
            "absent"
        })
        .arg(expected.unwrap_or_default())
        .arg(json)
        .arg(ttl)
        .invoke(conn)
        .map_err(|e| AgentBusError::Internal(format!("atomic presence update failed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support as backend_env;
    use chrono::{Duration, TimeZone, Utc};

    #[test]
    #[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL (disposable Redis)"]
    fn presence_cas_rejects_a_concurrent_local_announcement() {
        let client =
            redis::Client::open(backend_env::backend_url(backend_env::REDIS_URL_VAR)).unwrap();
        let mut pull = client
            .get_connection()
            .expect("disposable Redis unreachable");
        let mut local = client
            .get_connection()
            .expect("disposable Redis unreachable");
        let key = format!("agent_bus_test:presence_cas:{}", uuid::Uuid::new_v4());
        let incoming = serde_json::to_string(&presence(now(), 300, Some("cloud"))).unwrap();
        let newer =
            serde_json::to_string(&presence(now() + Duration::seconds(1), 300, None)).unwrap();
        for initial in [None, Some("old snapshot"), Some("")] {
            let _: () = local.del(&key).unwrap();
            if let Some(value) = initial {
                let _: () = local.set_ex(&key, value, 300).unwrap();
            }
            let snapshot: Option<String> = pull.get(&key).unwrap();
            // Deterministic interleaving: announce after GET, before CAS.
            let _: () = local.set_ex(&key, &newer, 300).unwrap();
            assert!(
                !compare_and_set_presence(&mut pull, &key, snapshot.as_deref(), &incoming, 300)
                    .unwrap()
            );
            assert_eq!(pull.get::<_, String>(&key).unwrap(), newer);
        }
        // An unchanged snapshot succeeds, including absence versus empty JSON.
        let _: () = local.del(&key).unwrap();
        assert!(compare_and_set_presence(&mut pull, &key, None, &incoming, 300).unwrap());
        let _: () = local.del(&key).unwrap();
    }

    #[test]
    #[ignore = "backend test: needs AGENT_BUS_TEST_REDIS_URL (disposable Redis)"]
    fn presence_contention_is_rechecked_and_retries_are_bounded() {
        let client =
            redis::Client::open(backend_env::backend_url(backend_env::REDIS_URL_VAR)).unwrap();
        let mut pull = client
            .get_connection()
            .expect("disposable Redis unreachable");
        let mut local = client
            .get_connection()
            .expect("disposable Redis unreachable");
        let mut settings = Settings::from_env();
        settings.presence_prefix =
            format!("agent_bus_test:presence_retry:{}:", uuid::Uuid::new_v4());
        let key = format!("{}agent-a", settings.presence_prefix);
        let incoming = presence(now(), 300, Some("cloud"));
        let newer =
            serde_json::to_string(&presence(now() + Duration::seconds(1), 300, None)).unwrap();
        let mut attempts = 0;
        let written = apply_synced_presence_with_cas(
            &mut pull,
            &settings,
            &incoming,
            "hub-a",
            now(),
            |conn, key, expected, json, ttl| {
                attempts += 1;
                let _: () = local.set_ex(key, &newer, 300).unwrap();
                compare_and_set_presence(conn, key, expected, json, ttl)
            },
        )
        .unwrap();
        assert!(
            !written,
            "the retry must see and preserve the newer local announcement"
        );
        assert_eq!(attempts, 1);
        assert_eq!(pull.get::<_, String>(&key).unwrap(), newer);
        let _: () = local.del(&key).unwrap();

        attempts = 0;
        let outcome = apply_synced_presence_with_cas(
            &mut pull,
            &settings,
            &incoming,
            "hub-a",
            now(),
            |conn, key, expected, json, ttl| {
                attempts += 1;
                let mut changed = presence(now() - Duration::seconds(1), 300, None);
                changed.session_id = format!("concurrent-{attempts}");
                let _: () = local
                    .set_ex(key, serde_json::to_string(&changed).unwrap(), 300)
                    .unwrap();
                compare_and_set_presence(conn, key, expected, json, ttl)
            },
        );
        assert!(
            outcome
                .unwrap_err()
                .to_string()
                .contains("retry on the next pull")
        );
        assert_eq!(attempts, PRESENCE_CAS_MAX_ATTEMPTS);
        let _: () = local.del(&key).unwrap();
    }

    fn presence(ts: chrono::DateTime<Utc>, ttl: u64, origin: Option<&str>) -> Presence {
        Presence {
            agent: "agent-a".to_owned(),
            status: "online".to_owned(),
            protocol_version: "1.0".to_owned(),
            timestamp_utc: ts.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
            session_id: "s".to_owned(),
            capabilities: vec![],
            metadata: origin.map_or_else(
                || serde_json::json!({}),
                |hub| serde_json::json!({ "origin_hub": hub }),
            ),
            ttl_seconds: ttl,
        }
    }

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 1, 12, 0, 0).unwrap()
    }

    #[test]
    fn newer_remote_row_is_written_with_its_remaining_ttl() {
        let incoming = presence(now() - Duration::seconds(10), 300, Some("cloud"));
        let local = presence(now() - Duration::seconds(60), 300, None);
        assert_eq!(
            synced_presence_ttl(&incoming, Some(&local), "hub-a", now()).unwrap(),
            Some(290)
        );
    }

    #[test]
    fn remote_row_never_overrides_a_newer_or_equal_local_row() {
        let incoming = presence(now() - Duration::seconds(30), 300, Some("cloud"));
        for local_age in [5, 30] {
            let local = presence(now() - Duration::seconds(local_age), 300, None);
            assert_eq!(
                synced_presence_ttl(&incoming, Some(&local), "hub-a", now()).unwrap(),
                None,
                "local row {local_age}s old must win"
            );
        }
    }

    #[test]
    fn remote_row_is_written_when_there_is_no_local_row() {
        let incoming = presence(now(), 120, Some("cloud"));
        assert_eq!(
            synced_presence_ttl(&incoming, None, "hub-a", now()).unwrap(),
            Some(120)
        );
    }

    #[test]
    fn expired_and_echoed_rows_are_dropped() {
        let expired = presence(now() - Duration::seconds(400), 300, Some("cloud"));
        assert_eq!(
            synced_presence_ttl(&expired, None, "hub-a", now()).unwrap(),
            None
        );
        let echo = presence(now(), 300, Some("hub-a"));
        assert_eq!(
            synced_presence_ttl(&echo, None, "hub-a", now()).unwrap(),
            None
        );
    }

    #[test]
    fn unparseable_timestamp_is_an_error_not_a_write() {
        let mut bad = presence(now(), 300, Some("cloud"));
        bad.timestamp_utc = "not a time".to_owned();
        assert!(synced_presence_ttl(&bad, None, "hub-a", now()).is_err());
    }

    #[test]
    fn only_rows_postgres_can_store_reach_the_writer() {
        let mut msg: Message = serde_json::from_value(serde_json::json!({
            "id": "0190a000-0000-7000-8000-000000000001",
            "timestamp_utc": "2026-01-01T00:00:00.000Z",
            "protocol_version": "1.0", "from": "a", "to": "b", "topic": "t", "body": "x"
        }))
        .unwrap();
        assert!(postgres_can_store(&msg));
        msg.id = "cloud-msg-1".to_owned();
        assert!(
            !postgres_can_store(&msg),
            "a non-UUID id would trip the PG breaker"
        );
        msg.id = "0190a000-0000-7000-8000-000000000001".to_owned();
        msg.timestamp_utc = "yesterday".to_owned();
        assert!(!postgres_can_store(&msg));
    }

    #[test]
    fn ingest_seen_key_is_scoped_by_origin() {
        assert_ne!(ingest_seen_key("hub-a", "1"), ingest_seen_key("hub-b", "1"));
    }
}
