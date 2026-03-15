//! `PostgreSQL` durable storage for messages and presence events.
//!
//! ## Connection reuse
//!
//! All write and read paths share a single process-lifetime [`PgClient`] held
//! inside a `Mutex<Option<PgClient>>`.  The "take / execute / return" pattern
//! keeps the mutex unlocked during the actual I/O so it never contends with
//! concurrent readers.  On any error the client is discarded and a fresh TCP
//! connection is made on the next call.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use postgres::{Client as PgClient, NoTls};
use uuid::Uuid;

use crate::models::{Message, Presence};
use crate::settings::Settings;

/// Maximum number of attempts for a transient `PostgreSQL` failure.
const PG_MAX_RETRIES: u32 = 3;

/// Base delay in milliseconds; doubles on each subsequent attempt (exponential back-off).
const PG_RETRY_BASE_MS: u64 = 50;

/// How long (in seconds) the circuit breaker suppresses retry attempts after a confirmed outage.
const PG_CIRCUIT_BREAKER_SECONDS: u64 = 60;

/// Returns the singleton that records the last instant `PostgreSQL` was confirmed unreachable.
fn pg_down_since() -> &'static Mutex<Option<Instant>> {
    static PG_DOWN: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    PG_DOWN.get_or_init(|| Mutex::new(None))
}

/// Returns `true` when the circuit breaker is open (i.e., PG was down within the last 60 s).
fn is_pg_circuit_open() -> bool {
    pg_down_since()
        .lock()
        .ok()
        .and_then(|guard| *guard)
        .is_some_and(|since| since.elapsed().as_secs() < PG_CIRCUIT_BREAKER_SECONDS)
}

/// Records that `PostgreSQL` is currently unreachable, opening the circuit breaker.
fn mark_pg_down() {
    if let Ok(mut guard) = pg_down_since().lock() {
        *guard = Some(Instant::now());
    }
}

/// Clears the circuit-breaker state, allowing future connection attempts.
fn mark_pg_up() {
    if let Ok(mut guard) = pg_down_since().lock() {
        *guard = None;
    }
}

/// Retries `operation` up to [`PG_MAX_RETRIES`] times with exponential back-off.
///
/// The closure is called repeatedly on failure, so it must be `FnMut`.  Each
/// attempt that fails logs a warning via `tracing`.  The final error from the
/// last attempt is returned if all attempts are exhausted.
///
/// # Examples
///
/// ```rust,ignore
/// let result = with_pg_retry(|| do_postgres_work(client));
/// ```
fn with_pg_retry<T>(mut operation: impl FnMut() -> Result<T>) -> Result<T> {
    if is_pg_circuit_open() {
        return Err(anyhow::anyhow!(
            "PostgreSQL circuit breaker open — skipping retry"
        ));
    }

    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 0..PG_MAX_RETRIES {
        match operation() {
            Ok(result) => {
                mark_pg_up();
                return Ok(result);
            }
            Err(e) => {
                tracing::warn!(
                    "PostgreSQL operation failed (attempt {}/{}): {e:#}",
                    attempt + 1,
                    PG_MAX_RETRIES
                );
                last_error = Some(e);
                if attempt + 1 < PG_MAX_RETRIES {
                    std::thread::sleep(std::time::Duration::from_millis(
                        PG_RETRY_BASE_MS * (1 << attempt),
                    ));
                }
            }
        }
    }
    mark_pg_down();
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("PostgreSQL operation failed after retries")))
}

pub(crate) fn run_postgres_blocking<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

/// Open a fresh `PostgreSQL` connection.
///
/// Used by the health probe, which intentionally bypasses the shared pool so
/// that it always verifies real network reachability.
pub(crate) fn connect_postgres(settings: &Settings) -> Result<Option<PgClient>> {
    let Some(database_url) = settings.database_url.as_deref() else {
        return Ok(None);
    };
    let client = PgClient::connect(database_url, NoTls).context("PostgreSQL connection failed")?;
    Ok(Some(client))
}

// ---------------------------------------------------------------------------
// Shared process-lifetime PG client pool (single connection, reused)
// ---------------------------------------------------------------------------

/// Singleton holding the shared `PostgreSQL` client between all calls.
///
/// `None` means no connection has been established yet (or the previous one
/// was discarded after an error).  Callers must *take* the `Option` out,
/// perform their work, and then *return* it via [`return_pg_client`].  This
/// keeps the `Mutex` unlocked during blocking I/O.
fn shared_pg_client() -> &'static Mutex<Option<PgClient>> {
    static PG_CLIENT: OnceLock<Mutex<Option<PgClient>>> = OnceLock::new();
    PG_CLIENT.get_or_init(|| Mutex::new(None))
}

/// Take the shared client from the pool, reconnecting lazily when absent.
///
/// Returns `None` (without error) when `database_url` is not configured.
/// The caller is responsible for either returning the client with
/// [`return_pg_client`] on success or simply dropping it on error so that
/// the next caller gets a fresh connection.
///
/// # Errors
///
/// Returns an error if the `Mutex` is poisoned or a new TCP connection to
/// `PostgreSQL` cannot be established.
fn get_pg_client(settings: &Settings) -> Result<Option<PgClient>> {
    let Some(database_url) = settings.database_url.as_deref() else {
        return Ok(None);
    };

    let mut guard = shared_pg_client()
        .lock()
        .map_err(|_e| anyhow::anyhow!("PostgreSQL shared-client mutex poisoned"))?;

    // Try to reuse the existing connection with a lightweight health ping.
    if let Some(ref mut client) = *guard {
        if client.simple_query("").is_ok() {
            // Connection is alive — take it out and hand to the caller.
            return Ok(guard.take());
        }
        // Connection is stale; drop it and fall through to reconnect.
        guard.take();
    }

    // Open a new connection and hand it directly to the caller (not back into
    // the slot) so we do not hold the mutex during connect.
    drop(guard); // release lock before the blocking TCP handshake
    let client =
        PgClient::connect(database_url, NoTls).context("PostgreSQL connection failed")?;
    Ok(Some(client))
}

/// Return a healthy client back to the shared pool after successful use.
///
/// If the mutex is poisoned the client is silently dropped; the next
/// call to [`get_pg_client`] will simply open a new connection.
fn return_pg_client(client: PgClient) {
    if let Ok(mut guard) = shared_pg_client().lock() {
        *guard = Some(client);
    }
}

pub(crate) fn storage_cache() -> &'static Mutex<HashSet<String>> {
    static STORAGE_READY: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    STORAGE_READY.get_or_init(|| Mutex::new(HashSet::new()))
}

pub(crate) fn storage_cache_key(settings: &Settings) -> Option<String> {
    settings.database_url.as_ref().map(|database_url| {
        format!(
            "{database_url}|{}|{}",
            settings.message_table, settings.presence_event_table
        )
    })
}

pub(crate) fn storage_ready(settings: &Settings) -> bool {
    let Some(cache_key) = storage_cache_key(settings) else {
        return false;
    };
    storage_cache()
        .lock()
        .map(|guard| guard.contains(&cache_key))
        .unwrap_or(false)
}

pub(crate) fn ensure_postgres_storage(client: &mut PgClient, settings: &Settings) -> Result<()> {
    let Some(cache_key) = storage_cache_key(settings) else {
        return Ok(());
    };
    if storage_ready(settings) {
        return Ok(());
    }

    client.batch_execute("create schema if not exists agent_bus")?;
    client.batch_execute(&format!(
        r"
        create table if not exists {message_table} (
            id uuid primary key,
            timestamp_utc timestamptz not null,
            protocol_version text not null default '1.0',
            sender text not null,
            recipient text not null,
            topic text not null,
            body text not null,
            thread_id text null,
            priority text not null,
            tags jsonb not null default '[]'::jsonb,
            request_ack boolean not null default false,
            reply_to text not null,
            metadata jsonb not null default '{{}}'::jsonb,
            stream_id text null
        );
        alter table {message_table} add column if not exists protocol_version text not null default '1.0';
        alter table {message_table} add column if not exists thread_id text null;
        alter table {message_table} add column if not exists stream_id text null;
        create index if not exists agent_bus_messages_recipient_ts_idx
            on {message_table} (recipient, timestamp_utc desc);
        create index if not exists agent_bus_messages_sender_ts_idx
            on {message_table} (sender, timestamp_utc desc);
        create index if not exists agent_bus_messages_topic_ts_idx
            on {message_table} (topic, timestamp_utc desc);
        create index if not exists agent_bus_messages_reply_to_idx
            on {message_table} (reply_to);
        create unique index if not exists agent_bus_messages_stream_id_idx
            on {message_table} (stream_id) where stream_id is not null;
        create index if not exists agent_bus_messages_tags_idx
            on {message_table} using gin (tags);

        create table if not exists {presence_event_table} (
            id bigserial primary key,
            timestamp_utc timestamptz not null,
            protocol_version text not null default '1.0',
            agent text not null,
            status text not null,
            session_id text not null,
            capabilities jsonb not null default '[]'::jsonb,
            metadata jsonb not null default '{{}}'::jsonb,
            ttl_seconds bigint not null
        );
        alter table {presence_event_table} add column if not exists protocol_version text not null default '1.0';
        create index if not exists agent_bus_presence_events_agent_ts_idx
            on {presence_event_table} (agent, timestamp_utc desc);
        ",
        message_table = settings.message_table,
        presence_event_table = settings.presence_event_table,
    ))?;

    if let Ok(mut guard) = storage_cache().lock() {
        guard.insert(cache_key);
    }
    Ok(())
}

pub(crate) fn parse_timestamp_utc(timestamp_utc: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(&timestamp_utc.replace('Z', "+00:00"))
        .with_context(|| format!("invalid timestamp_utc: {timestamp_utc}"))?;
    Ok(parsed.with_timezone(&Utc))
}

pub(crate) fn persist_message_postgres(settings: &Settings, message: &Message) -> Result<()> {
    with_pg_retry(|| {
        run_postgres_blocking(|| {
            let Some(mut client) = get_pg_client(settings)? else {
                return Ok(());
            };
            ensure_postgres_storage(&mut client, settings)?;

            let message_id = Uuid::parse_str(&message.id)
                .with_context(|| format!("invalid message id: {}", message.id))?;
            let timestamp_utc = parse_timestamp_utc(&message.timestamp_utc)?;
            let tags = serde_json::Value::Array(
                message
                    .tags
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            );
            let reply_to = message.reply_to.clone().unwrap_or_default();

            client.execute(
                &format!(
                    "insert into {} \
                     (id, timestamp_utc, protocol_version, sender, recipient, topic, body, thread_id, priority, tags, request_ack, reply_to, metadata, stream_id) \
                     values ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
                     on conflict (id) do nothing",
                    settings.message_table
                ),
                &[
                    &message_id,
                    &timestamp_utc,
                    &message.protocol_version,
                    &message.from,
                    &message.to,
                    &message.topic,
                    &message.body,
                    &message.thread_id,
                    &message.priority,
                    &tags,
                    &message.request_ack,
                    &reply_to,
                    &message.metadata,
                    &message.stream_id,
                ],
            )?;
            // Return the healthy connection to the pool.
            return_pg_client(client);
            Ok(())
        })
    })
}

pub(crate) fn persist_presence_postgres(settings: &Settings, presence: &Presence) -> Result<()> {
    with_pg_retry(|| {
        run_postgres_blocking(|| {
            let Some(mut client) = get_pg_client(settings)? else {
                return Ok(());
            };
            ensure_postgres_storage(&mut client, settings)?;

            let timestamp_utc = parse_timestamp_utc(&presence.timestamp_utc)?;
            let capabilities = serde_json::Value::Array(
                presence
                    .capabilities
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            );
            let ttl_seconds =
                i64::try_from(presence.ttl_seconds).context("ttl_seconds exceeds i64")?;

            client.execute(
                &format!(
                    "insert into {} \
                     (timestamp_utc, protocol_version, agent, status, session_id, capabilities, metadata, ttl_seconds) \
                     values ($1, $2, $3, $4, $5, $6, $7, $8)",
                    settings.presence_event_table
                ),
                &[
                    &timestamp_utc,
                    &presence.protocol_version,
                    &presence.agent,
                    &presence.status,
                    &presence.session_id,
                    &capabilities,
                    &presence.metadata,
                    &ttl_seconds,
                ],
            )?;
            // Return the healthy connection to the pool.
            return_pg_client(client);
            Ok(())
        })
    })
}

/// Backfill missing Redis messages into `PostgreSQL`.
///
/// Iterates `messages` (typically the full Redis stream) and inserts each one
/// using `ON CONFLICT (id) DO NOTHING`, making the operation safe to repeat.
///
/// Returns `(total_checked, newly_inserted)`.
///
/// # Errors
///
/// Returns an error if the database connection or any `INSERT` fails.
pub(crate) fn sync_redis_to_postgres(
    settings: &Settings,
    messages: &[Message],
) -> Result<(usize, usize)> {
    run_postgres_blocking(|| {
        let Some(mut client) = get_pg_client(settings)? else {
            return Ok((0, 0));
        };
        ensure_postgres_storage(&mut client, settings)?;

        let mut inserted: usize = 0;
        let mut skipped: usize = 0;
        for msg in messages {
            // Skip messages with empty or non-UUID IDs (legacy Python CLI entries)
            let Ok(message_id) = Uuid::parse_str(&msg.id) else {
                skipped += 1;
                continue;
            };
            let Ok(timestamp_utc) = parse_timestamp_utc(&msg.timestamp_utc) else {
                skipped += 1;
                continue;
            };
            let tags = serde_json::Value::Array(
                msg.tags.iter().cloned().map(serde_json::Value::String).collect(),
            );
            let reply_to = msg.reply_to.clone().unwrap_or_default();

            let rows = client.execute(
                &format!(
                    "INSERT INTO {} \
                     (id, timestamp_utc, protocol_version, sender, recipient, topic, body, \
                      thread_id, priority, tags, request_ack, reply_to, metadata, stream_id) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
                     ON CONFLICT (id) DO NOTHING",
                    settings.message_table
                ),
                &[
                    &message_id,
                    &timestamp_utc,
                    &msg.protocol_version,
                    &msg.from,
                    &msg.to,
                    &msg.topic,
                    &msg.body,
                    &msg.thread_id,
                    &msg.priority,
                    &tags,
                    &msg.request_ack,
                    &reply_to,
                    &msg.metadata,
                    &msg.stream_id,
                ],
            )?;
            if rows > 0 {
                inserted += 1;
            }
        }

        return_pg_client(client);
        Ok((messages.len(), inserted))
    })
}

pub(crate) fn parse_tags(value: &serde_json::Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn row_to_message(row: &postgres::Row) -> Message {
    Message {
        id: row.get::<_, Uuid>("id").to_string(),
        timestamp_utc: row
            .get::<_, DateTime<Utc>>("timestamp_utc")
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string(),
        protocol_version: row.get("protocol_version"),
        from: row.get("sender"),
        to: row.get("recipient"),
        topic: row.get("topic"),
        body: row.get("body"),
        thread_id: row.get("thread_id"),
        tags: parse_tags(&row.get::<_, serde_json::Value>("tags")),
        priority: row.get("priority"),
        request_ack: row.get("request_ack"),
        reply_to: {
            let reply_to: String = row.get("reply_to");
            if reply_to.is_empty() {
                None
            } else {
                Some(reply_to)
            }
        },
        metadata: row.get("metadata"),
        stream_id: row.get("stream_id"),
    }
}

pub(crate) fn list_messages_postgres(
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    include_broadcast: bool,
) -> Result<Vec<Message>> {
    run_postgres_blocking(|| {
        let Some(mut client) = get_pg_client(settings)? else {
            return Ok(Vec::new());
        };
        ensure_postgres_storage(&mut client, settings)?;

        let since_minutes = i64::try_from(since_minutes).context("since_minutes exceeds i64")?;
        let limit = i64::try_from(limit).context("limit exceeds i64")?;
        let agent_filter = agent.map(str::to_owned);
        let sender_filter = from_agent.map(str::to_owned);

        let rows = client.query(
            &format!(
                "select id, timestamp_utc, protocol_version, sender, recipient, topic, body, thread_id, tags, priority, request_ack, reply_to, metadata, stream_id \
                 from {} \
                 where timestamp_utc >= now() - ($1::bigint * interval '1 minute') \
                   and ($2::text is null or sender = $2) \
                   and ($3::text is null or recipient = $3 or ($4 and recipient = 'all')) \
                 order by timestamp_utc desc \
                 limit $5",
                settings.message_table
            ),
            &[&since_minutes, &sender_filter, &agent_filter, &include_broadcast, &limit],
        )?;

        let mut messages: Vec<Message> = rows.iter().map(row_to_message).collect();
        messages.reverse();
        return_pg_client(client);
        Ok(messages)
    })
}

/// Query messages whose `tags` array contains `tag` (uses the GIN index).
///
/// Returns up to `limit` records within the `since_minutes` window, in
/// chronological order. Returns an empty `Vec` when `PostgreSQL` is not
/// configured or the tag matches nothing.
///
/// # Errors
///
/// Returns an error if the database operation fails.
pub(crate) fn list_messages_by_tag(
    settings: &Settings,
    tag: &str,
    since_minutes: u64,
    limit: usize,
) -> Result<Vec<Message>> {
    run_postgres_blocking(|| {
        let Some(mut client) = get_pg_client(settings)? else {
            return Ok(Vec::new());
        };
        ensure_postgres_storage(&mut client, settings)?;
        let since = i64::try_from(since_minutes).context("since_minutes exceeds i64")?;
        let limit_i64 = i64::try_from(limit).context("limit exceeds i64")?;
        let tag_json = serde_json::json!([tag]);
        let rows = client.query(
            &format!(
                "SELECT id, timestamp_utc, protocol_version, sender, recipient, topic, body, \
                 thread_id, tags, priority, request_ack, reply_to, metadata, stream_id \
                 FROM {} \
                 WHERE timestamp_utc >= now() - ($1::bigint * interval '1 minute') \
                   AND tags @> $2::jsonb \
                 ORDER BY timestamp_utc DESC \
                 LIMIT $3",
                settings.message_table
            ),
            &[&since, &tag_json, &limit_i64],
        )?;
        let mut messages: Vec<Message> = rows.iter().map(row_to_message).collect();
        messages.reverse();
        return_pg_client(client);
        Ok(messages)
    })
}

pub(crate) fn count_messages_postgres(settings: &Settings) -> Option<i64> {
    run_postgres_blocking(|| {
        let Some(mut client) = get_pg_client(settings)? else {
            return Ok(None);
        };
        let row = client.query_one(
            &format!("select count(*) as cnt from {}", settings.message_table),
            &[],
        )?;
        let count = row.get::<_, i64>("cnt");
        return_pg_client(client);
        Ok(Some(count))
    })
    .ok()
    .flatten()
}

pub(crate) fn count_presence_postgres(settings: &Settings) -> Option<i64> {
    run_postgres_blocking(|| {
        let Some(mut client) = get_pg_client(settings)? else {
            return Ok(None);
        };
        let row = client.query_one(
            &format!(
                "select count(*) as cnt from {}",
                settings.presence_event_table
            ),
            &[],
        )?;
        let count = row.get::<_, i64>("cnt");
        return_pg_client(client);
        Ok(Some(count))
    })
    .ok()
    .flatten()
}

/// Delete messages older than `older_than_days` days from `PostgreSQL`.
///
/// Returns the number of rows deleted, or `0` if `PostgreSQL` is not configured.
///
/// # Errors
///
/// Returns an error if the database operation fails.
pub(crate) fn prune_old_messages(settings: &Settings, older_than_days: u64) -> Result<u64> {
    run_postgres_blocking(|| {
        let Some(mut client) = get_pg_client(settings)? else {
            return Ok(0);
        };
        ensure_postgres_storage(&mut client, settings)?;
        let days = i64::try_from(older_than_days).context("days exceeds i64")?;
        let rows = client.execute(
            &format!(
                "delete from {} where timestamp_utc < now() - ($1::bigint * interval '1 day')",
                settings.message_table
            ),
            &[&days],
        )?;
        return_pg_client(client);
        Ok(rows)
    })
}

/// Delete presence events older than `older_than_days` days from `PostgreSQL`.
///
/// Returns the number of rows deleted, or `0` if `PostgreSQL` is not configured.
///
/// # Errors
///
/// Returns an error if the database operation fails.
pub(crate) fn prune_old_presence(settings: &Settings, older_than_days: u64) -> Result<u64> {
    run_postgres_blocking(|| {
        let Some(mut client) = get_pg_client(settings)? else {
            return Ok(0);
        };
        ensure_postgres_storage(&mut client, settings)?;
        let days = i64::try_from(older_than_days).context("days exceeds i64")?;
        let rows = client.execute(
            &format!(
                "delete from {} where timestamp_utc < now() - ($1::bigint * interval '1 day')",
                settings.presence_event_table
            ),
            &[&days],
        )?;
        return_pg_client(client);
        Ok(rows)
    })
}

fn row_to_presence(row: &postgres::Row) -> Presence {
    Presence {
        agent: row.get("agent"),
        status: row.get("status"),
        protocol_version: row.get("protocol_version"),
        timestamp_utc: row
            .get::<_, DateTime<Utc>>("timestamp_utc")
            .format("%Y-%m-%dT%H:%M:%S%.6fZ")
            .to_string(),
        session_id: row.get("session_id"),
        capabilities: parse_tags(&row.get::<_, serde_json::Value>("capabilities")),
        metadata: row.get("metadata"),
        #[expect(
            clippy::cast_sign_loss,
            reason = "ttl_seconds stored as i64 in PostgreSQL; negative values treated as 0"
        )]
        ttl_seconds: row.get::<_, i64>("ttl_seconds").max(0) as u64,
    }
}

/// Query historical presence events from `PostgreSQL`.
///
/// Returns up to `limit` records within the `since_minutes` window, newest first.
/// Returns an empty `Vec` if `PostgreSQL` is not configured.
///
/// # Errors
///
/// Returns an error if the database operation fails.
pub(crate) fn list_presence_history_postgres(
    settings: &Settings,
    agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
) -> Result<Vec<Presence>> {
    run_postgres_blocking(|| {
        let Some(mut client) = get_pg_client(settings)? else {
            return Ok(Vec::new());
        };
        ensure_postgres_storage(&mut client, settings)?;
        let since_minutes = i64::try_from(since_minutes).context("since_minutes exceeds i64")?;
        let limit = i64::try_from(limit).context("limit exceeds i64")?;
        let agent_filter = agent.map(str::to_owned);
        let rows = client.query(
            &format!(
                "select timestamp_utc, protocol_version, agent, status, session_id, capabilities, metadata, ttl_seconds \
                 from {} \
                 where timestamp_utc >= now() - ($1::bigint * interval '1 minute') \
                   and ($2::text is null or agent = $2) \
                 order by timestamp_utc desc \
                 limit $3",
                settings.presence_event_table
            ),
            &[&since_minutes, &agent_filter, &limit],
        )?;
        let results: Vec<Presence> = rows.iter().map(row_to_presence).collect();
        return_pg_client(client);
        Ok(results)
    })
}

pub(crate) fn probe_postgres(settings: &Settings) -> (Option<bool>, Option<String>, bool) {
    if settings.database_url.is_none() {
        return (None, None, false);
    }
    match run_postgres_blocking(|| {
        // Deliberately use a fresh connection here so that the health probe
        // verifies real TCP reachability rather than returning a cached result.
        let Some(mut client) = connect_postgres(settings)? else {
            return Ok((None, None, false));
        };
        ensure_postgres_storage(&mut client, settings)?;
        mark_pg_up();
        // Seed the shared pool so the next write reuses this connection.
        return_pg_client(client);
        Ok((Some(true), None, true))
    }) {
        Ok(result) => result,
        Err(error) => (
            Some(false),
            Some(format!("{error:#}")),
            storage_ready(settings),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `get_pg_client` must return `Ok(None)` immediately when `database_url`
    /// is not set — no TCP attempt, no mutex contention.
    #[test]
    fn get_pg_client_returns_none_when_no_database_url() {
        let mut settings = Settings::from_env();
        settings.database_url = None;
        let result = get_pg_client(&settings);
        assert!(result.is_ok(), "expected Ok from get_pg_client");
        assert!(
            result.unwrap().is_none(),
            "expected None client when database_url is absent"
        );
    }

    /// `return_pg_client` followed by `get_pg_client` with no `database_url`
    /// must not panic or deadlock — the pool slot is simply ignored when PG
    /// is not configured.
    #[test]
    fn shared_pool_is_inert_without_database_url() {
        // The pool is a global singleton; we test the no-op path only.
        let mut settings = Settings::from_env();
        settings.database_url = None;

        // Both calls must be infallible.
        let client_opt = get_pg_client(&settings).expect("get_pg_client should not error");
        assert!(client_opt.is_none());
        // return_pg_client with no actual client — nothing to return; confirm
        // the guard can still be acquired (not poisoned).
        assert!(
            shared_pg_client().lock().is_ok(),
            "shared pool mutex must not be poisoned"
        );
    }

    #[test]
    fn pg_circuit_breaker_skips_when_open() {
        // Ensure a clean state before the test.
        mark_pg_up();
        assert!(
            !is_pg_circuit_open(),
            "circuit should be closed after mark_pg_up"
        );

        mark_pg_down();
        assert!(
            is_pg_circuit_open(),
            "circuit should be open after mark_pg_down"
        );

        // with_pg_retry must short-circuit immediately while the breaker is open.
        let mut calls = 0_u32;
        let result: Result<()> = with_pg_retry(|| {
            calls += 1;
            Ok(())
        });
        assert!(result.is_err(), "expected circuit-breaker error");
        assert_eq!(
            calls, 0,
            "operation must not be called while circuit is open"
        );

        mark_pg_up();
        assert!(
            !is_pg_circuit_open(),
            "circuit should be closed again after mark_pg_up"
        );
    }
}
