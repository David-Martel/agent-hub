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
/// command fails. On a failed `XADD` the dedup marker is removed so a retry
/// can succeed.
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

    let seen_key = ingest_seen_key(origin_hub, &msg.id);
    let fresh: bool = redis::cmd("SET")
        .arg(&seen_key)
        .arg("1")
        .arg("NX")
        .arg("EX")
        .arg(INGEST_SEEN_TTL_SECONDS)
        .query::<Option<String>>(conn)
        .map(|reply| reply.is_some())
        .map_err(|e| AgentBusError::Internal(format!("SET NX failed: {e}")))?;
    if !fresh {
        return Ok(IngestOutcome::Duplicate);
    }

    match xadd_synced(conn, settings, msg, origin_hub) {
        Ok(stream_id) => {
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
        Err(error) => {
            let _: std::result::Result<(), _> = conn.del(&seen_key);
            Err(error)
        }
    }
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
) -> Result<String> {
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
    redis::cmd("XADD")
        .arg(&settings.stream_key)
        .arg("MAXLEN")
        .arg("~")
        .arg(settings.stream_maxlen)
        .arg("*")
        .arg(&fields)
        .query(conn)
        .map_err(|e| AgentBusError::Internal(format!("XADD (synced) failed: {e}")))
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
    let key = format!("{}{}", settings.presence_prefix, presence.agent);
    let existing: Option<String> = conn
        .get(&key)
        .map_err(|e| AgentBusError::Internal(format!("GET presence failed: {e}")))?;
    let local = existing.and_then(|json| serde_json::from_str::<Presence>(&json).ok());
    let Some(ttl) = synced_presence_ttl(presence, local.as_ref(), local_hub, now)? else {
        return Ok(false);
    };
    let json = serde_json::to_string(presence)
        .map_err(|e| AgentBusError::Internal(format!("serialize presence: {e}")))?;
    let _: () = conn
        .set_ex(&key, json, ttl)
        .map_err(|e| AgentBusError::Internal(format!("SET EX failed: {e}")))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};

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
