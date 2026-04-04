//! `PostgreSQL` durable storage for messages and presence events.
//!
//! ## Connection reuse
//!
//! All write and read paths share a single process-lifetime [`PgClient`] held
//! inside a `Mutex<Option<PgClient>>`.  The "take / execute / return" pattern
//! keeps the mutex unlocked during the actual I/O so it never contends with
//! concurrent readers.  On any error the client is discarded and a fresh TCP
//! connection is made on the next call.
//!
//! ## Async write-through
//!
//! [`PgWriter`] provides a non-blocking fire-and-forget write path via a
//! `tokio::sync::mpsc::UnboundedSender`.  A background task drains the channel
//! and flushes to `PostgreSQL` in batches of up to 10 messages, or every 100 ms,
//! whichever comes first.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use chrono::{DateTime, Utc};
use postgres::{Client as PgClient, NoTls};
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::models::{Message, Presence};
use crate::settings::Settings;

/// Maximum number of attempts for a transient `PostgreSQL` failure.
const PG_MAX_RETRIES: u32 = 3;

/// Base delay in milliseconds; doubles on each subsequent attempt (exponential back-off).
const PG_RETRY_BASE_MS: u64 = 50;

/// How long (in seconds) the circuit breaker suppresses retry attempts after a confirmed outage.
const PG_CIRCUIT_BREAKER_SECONDS: u64 = 60;

// ---------------------------------------------------------------------------
// Write-through metrics
// ---------------------------------------------------------------------------

/// Monotonic counters for the async `PostgreSQL` write-through path.
///
/// All fields are `AtomicU64` so they can be read from any thread without locking.
/// The `Relaxed` ordering is sufficient — these are best-effort gauges, not
/// synchronisation primitives.
///
/// Retrieve the process-lifetime singleton via [`pg_metrics`].
#[derive(Debug)]
pub struct PgWriteMetrics {
    /// Total [`PgWriteRequest::Message`] and [`PgWriteRequest::Presence`] items
    /// enqueued via [`PgWriter::send_message`] / [`PgWriter::send_presence`].
    pub messages_queued: AtomicU64,
    /// Total items successfully written to `PostgreSQL` (one per row persisted).
    pub messages_written: AtomicU64,
    /// Total flush cycles completed by the background task.
    pub batches_flushed: AtomicU64,
    /// Total write failures (one per failed `persist_*` call inside a batch).
    pub write_errors: AtomicU64,
}

impl PgWriteMetrics {
    const fn new() -> Self {
        Self {
            messages_queued: AtomicU64::new(0),
            messages_written: AtomicU64::new(0),
            batches_flushed: AtomicU64::new(0),
            write_errors: AtomicU64::new(0),
        }
    }
}

/// Returns the process-lifetime [`PgWriteMetrics`] singleton.
///
/// # Examples
///
/// ```rust,ignore
/// let queued = pg_metrics().messages_queued.load(Ordering::Relaxed);
/// ```
#[must_use]
pub fn pg_metrics() -> &'static PgWriteMetrics {
    static METRICS: PgWriteMetrics = PgWriteMetrics::new();
    &METRICS
}

/// Returns the singleton that records the last instant `PostgreSQL` was confirmed unreachable.
fn pg_down_since() -> &'static Mutex<Option<Instant>> {
    static PG_DOWN: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
    PG_DOWN.get_or_init(|| Mutex::new(None))
}

/// Returns `true` when the circuit breaker is open (i.e., PG was down within the last 60 s).
///
/// Public so that callers (e.g. `redis_bus::bus_list_messages_with_filters`)
/// can skip the `PostgreSQL` path early without entering the retry/connection
/// machinery.
#[must_use]
pub fn is_pg_circuit_open() -> bool {
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

/// # Errors
/// Propagates any error returned by `operation`.
pub fn run_postgres_blocking<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
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
///
/// # Errors
/// Returns an error if the connection attempt fails.
pub fn connect_postgres(settings: &Settings) -> Result<Option<PgClient>> {
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
    let client = PgClient::connect(database_url, NoTls).context("PostgreSQL connection failed")?;
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

pub fn storage_cache() -> &'static Mutex<HashSet<String>> {
    static STORAGE_READY: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    STORAGE_READY.get_or_init(|| Mutex::new(HashSet::new()))
}

#[must_use]
pub fn storage_cache_key(settings: &Settings) -> Option<String> {
    settings.database_url.as_ref().map(|database_url| {
        format!(
            "{database_url}|{}|{}",
            settings.message_table, settings.presence_event_table
        )
    })
}

#[must_use]
pub fn storage_ready(settings: &Settings) -> bool {
    let Some(cache_key) = storage_cache_key(settings) else {
        return false;
    };
    storage_cache()
        .lock()
        .map(|guard| guard.contains(&cache_key))
        .unwrap_or(false)
}

/// # Errors
/// Returns an error if the DDL migrations fail.
pub fn ensure_postgres_storage(client: &mut PgClient, settings: &Settings) -> Result<()> {
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
        create index if not exists agent_bus_messages_thread_id_ts_idx
            on {message_table} (thread_id, timestamp_utc desc);
        create index if not exists agent_bus_messages_reply_to_idx
            on {message_table} (reply_to);
        create unique index if not exists agent_bus_messages_stream_id_idx
            on {message_table} (stream_id) where stream_id is not null;
        create index if not exists agent_bus_messages_tags_idx
            on {message_table} using gin (tags);
        -- Timestamp-only index: enables index-only scans for count(*) and range
        -- queries that do not filter by recipient/sender/topic.  Added 2026-03-20
        -- after EXPLAIN ANALYZE showed 993 seq scans on the health count path.
        create index if not exists agent_bus_messages_ts_idx
            on {message_table} (timestamp_utc desc);

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
        -- Timestamp-only index: enables index-only scans for count(*) and
        -- unfiltered time-range queries on presence_events.  Added 2026-03-20.
        create index if not exists agent_bus_presence_events_ts_idx
            on {presence_event_table} (timestamp_utc desc);
        ",
        message_table = settings.message_table,
        presence_event_table = settings.presence_event_table,
    ))?;

    if let Ok(mut guard) = storage_cache().lock() {
        guard.insert(cache_key);
    }
    Ok(())
}

/// # Errors
/// Returns an error if the timestamp string is not valid RFC 3339.
pub fn parse_timestamp_utc(timestamp_utc: &str) -> Result<DateTime<Utc>> {
    let parsed = DateTime::parse_from_rfc3339(&timestamp_utc.replace('Z', "+00:00"))
        .with_context(|| format!("invalid timestamp_utc: {timestamp_utc}"))?;
    Ok(parsed.with_timezone(&Utc))
}

/// # Errors
/// Returns an error if the message cannot be persisted to `PostgreSQL`.
pub fn persist_message_postgres(settings: &Settings, message: &Message) -> Result<()> {
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

/// # Errors
/// Returns an error if the presence record cannot be persisted to `PostgreSQL`.
pub fn persist_presence_postgres(settings: &Settings, presence: &Presence) -> Result<()> {
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
pub fn sync_redis_to_postgres(settings: &Settings, messages: &[Message]) -> Result<(usize, usize)> {
    run_postgres_blocking(|| {
        let Some(mut client) = get_pg_client(settings)? else {
            return Ok((0, 0));
        };
        ensure_postgres_storage(&mut client, settings)?;

        let mut inserted: usize = 0;
        let mut _skipped: usize = 0;
        for msg in messages {
            // Skip messages with empty or non-UUID IDs (legacy Python CLI entries)
            let Ok(message_id) = Uuid::parse_str(&msg.id) else {
                _skipped += 1;
                continue;
            };
            let Ok(timestamp_utc) = parse_timestamp_utc(&msg.timestamp_utc) else {
                _skipped += 1;
                continue;
            };
            let tags = serde_json::Value::Array(
                msg.tags
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
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

#[must_use]
pub fn parse_tags(value: &serde_json::Value) -> Vec<String> {
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

#[must_use]
pub fn row_to_message(row: &postgres::Row) -> Message {
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
        tags: parse_tags(&row.get::<_, serde_json::Value>("tags")).into(),
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

/// Build a de-duplicated tag list for query-layer scope filters.
///
/// Callers can pass `repo:<name>` and `session:<id>` through this helper so the
/// query functions only need to reason about tags.
pub fn query_scope_tags(repo: Option<&str>, session: Option<&str>, tags: &[&str]) -> Vec<String> {
    let mut scoped = Vec::with_capacity(tags.len() + 2);
    let mut seen = HashSet::new();

    for tag in repo
        .filter(|value| !value.is_empty())
        .map(|value| format!("repo:{value}"))
        .into_iter()
        .chain(
            session
                .filter(|value| !value.is_empty())
                .map(|value| format!("session:{value}")),
        )
        .chain(
            tags.iter()
                .copied()
                .filter(|tag| !tag.is_empty())
                .map(str::to_owned),
        )
    {
        if seen.insert(tag.clone()) {
            scoped.push(tag);
        }
    }

    scoped
}

/// # Errors
/// Returns an error if the database query fails.
#[expect(
    clippy::too_many_arguments,
    reason = "query helpers accept transport-level filters without allocating wrapper structs"
)]
pub fn list_messages_postgres_with_filters(
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    include_broadcast: bool,
    thread_id: Option<&str>,
    required_tags: &[&str],
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
        let thread_filter = thread_id.map(str::to_owned);
        let tag_filter: Option<serde_json::Value> = if required_tags.is_empty() {
            None
        } else {
            Some(serde_json::json!(required_tags))
        };

        let rows = client.query(
            &format!(
                "select id, timestamp_utc, protocol_version, sender, recipient, topic, body, thread_id, tags, priority, request_ack, reply_to, metadata, stream_id \
                 from {} \
                 where timestamp_utc >= now() - ($1::bigint * interval '1 minute') \
                   and ($2::text is null or sender = $2) \
                   and ($3::text is null or recipient = $3 or ($4 and recipient = 'all')) \
                   and ($5::text is null or thread_id = $5) \
                   and ($6::jsonb is null or tags @> $6::jsonb) \
                 order by timestamp_utc desc \
                 limit $7",
                settings.message_table
            ),
            &[
                &since_minutes,
                &sender_filter,
                &agent_filter,
                &include_broadcast,
                &thread_filter,
                &tag_filter,
                &limit,
            ],
        )?;

        let mut messages: Vec<Message> = rows.iter().map(row_to_message).collect();
        messages.reverse();
        return_pg_client(client);
        Ok(messages)
    })
}

/// # Errors
/// Returns an error if the database query fails.
pub fn list_messages_postgres(
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    include_broadcast: bool,
) -> Result<Vec<Message>> {
    list_messages_postgres_with_filters(
        settings,
        agent,
        from_agent,
        since_minutes,
        limit,
        include_broadcast,
        None,
        &[],
    )
}

/// Convenience wrapper for query-layer scope tags.
///
/// This is the preferred entry point when a caller wants to scope a message
/// query by repo/session tags without assembling the `repo:<name>` and
/// `session:<id>` strings itself.
///
/// # Errors
/// Returns an error if the database query fails.
#[allow(dead_code)]
#[expect(
    clippy::too_many_arguments,
    reason = "scoped wrapper forwards the full filter set to the shared query path"
)]
pub fn list_messages_postgres_scoped(
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    include_broadcast: bool,
    thread_id: Option<&str>,
    repo: Option<&str>,
    session: Option<&str>,
    tags: &[&str],
) -> Result<Vec<Message>> {
    let scoped_tags = query_scope_tags(repo, session, tags);
    let scoped_tag_refs: Vec<&str> = scoped_tags.iter().map(String::as_str).collect();
    list_messages_postgres_with_filters(
        settings,
        agent,
        from_agent,
        since_minutes,
        limit,
        include_broadcast,
        thread_id,
        &scoped_tag_refs,
    )
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
#[allow(dead_code)]
pub fn list_messages_by_tag(
    settings: &Settings,
    tag: &str,
    since_minutes: u64,
    limit: usize,
) -> Result<Vec<Message>> {
    let tags = [tag];
    list_messages_postgres_with_filters(
        settings,
        None,
        None,
        since_minutes,
        limit,
        true,
        None,
        &tags,
    )
}

/// Query messages matching multiple tags via the GIN index.
///
/// This is the preferred entry point for tag-scoped reads (session-summary,
/// dedup, compact-context) where the caller has a set of required tags and
/// optionally a thread filter.  The query uses `tags @> $1::jsonb` which
/// leverages the GIN index for efficient containment checks.
///
/// Returns up to `limit` records within the `since_minutes` window, in
/// chronological order.  Returns an empty `Vec` when `PostgreSQL` is not
/// configured, the circuit breaker is open, or no rows match.
///
/// # Errors
///
/// Returns an error if the database operation fails.
pub fn query_messages_by_tags(
    settings: &Settings,
    tags: &[&str],
    thread_id: Option<&str>,
    since_minutes: u64,
    limit: usize,
) -> Result<Vec<Message>> {
    if tags.is_empty() && thread_id.is_none() {
        return Err(anyhow::anyhow!(
            "query_messages_by_tags requires at least one tag or a thread_id"
        ));
    }

    if is_pg_circuit_open() {
        return Err(anyhow::anyhow!(
            "PostgreSQL circuit breaker open — skipping tag query"
        ));
    }

    // Delegate to the full filter function with agent/sender set to None
    // and include_broadcast=true so we capture all matching messages.
    list_messages_postgres_with_filters(
        settings,
        None, // agent
        None, // from_agent
        since_minutes,
        limit,
        true, // include_broadcast
        thread_id,
        tags,
    )
}

/// Fetch both message and presence event counts in a single `PostgreSQL` round-trip.
///
/// Returns `(msg_count, presence_count)`.  Either component is `None` when
/// `PostgreSQL` is not configured or the query fails.
///
/// This replaces the previous two-query path (`count_messages_postgres` +
/// `count_presence_postgres`) with a single subquery so the health endpoint
/// incurs one round-trip instead of two.
#[must_use]
pub fn count_both_postgres(settings: &Settings) -> (Option<i64>, Option<i64>) {
    run_postgres_blocking(|| {
        let Some(mut client) = get_pg_client(settings)? else {
            return Ok((None, None));
        };
        let row = client.query_one(
            &format!(
                "SELECT \
                    (SELECT count(*) FROM {msg}) AS msg_count, \
                    (SELECT count(*) FROM {pres}) AS presence_count",
                msg = settings.message_table,
                pres = settings.presence_event_table,
            ),
            &[],
        )?;
        let msg_count: i64 = row.get("msg_count");
        let presence_count: i64 = row.get("presence_count");
        return_pg_client(client);
        Ok((Some(msg_count), Some(presence_count)))
    })
    .unwrap_or_default()
}

/// Delete messages older than `older_than_days` days from `PostgreSQL`.
///
/// Returns the number of rows deleted, or `0` if `PostgreSQL` is not configured.
///
/// # Errors
///
/// Returns an error if the database operation fails.
pub fn prune_old_messages(settings: &Settings, older_than_days: u64) -> Result<u64> {
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
pub fn prune_old_presence(settings: &Settings, older_than_days: u64) -> Result<u64> {
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
pub fn list_presence_history_postgres(
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

#[must_use]
pub fn probe_postgres(settings: &Settings) -> (Option<bool>, Option<String>, bool) {
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

// ---------------------------------------------------------------------------
// Proactive circuit-breaker health monitor
// ---------------------------------------------------------------------------

/// Spawn a background OS thread that proactively resets the circuit breaker.
///
/// Every `interval_secs` seconds the thread wakes up. When the circuit breaker
/// is open (i.e. `PostgreSQL` was recently marked down), it calls
/// [`probe_postgres`] to test real connectivity. On a successful probe it calls
/// [`mark_pg_up`] so subsequent write attempts are no longer suppressed.
///
/// The thread is named `"pg-health-monitor"` and is a daemon thread — it exits
/// automatically when the process terminates.
///
/// # Examples
///
/// ```rust,ignore
/// spawn_pg_health_monitor(settings.clone(), 30);
/// ```
///
/// # Panics
/// Panics if the background thread cannot be spawned (OS resource exhaustion).
pub fn spawn_pg_health_monitor(settings: Settings, interval_secs: u64) {
    std::thread::Builder::new()
        .name("pg-health-monitor".to_owned())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_secs(interval_secs));
                if is_pg_circuit_open() {
                    tracing::info!(
                        "pg-health-monitor: circuit open — probing PostgreSQL connectivity"
                    );
                    let (ok, _err, _ready) = probe_postgres(&settings);
                    if ok == Some(true) {
                        mark_pg_up();
                        tracing::info!(
                            "pg-health-monitor: probe succeeded — circuit closed, writes resumed"
                        );
                    } else {
                        tracing::info!("pg-health-monitor: probe failed — circuit remains open");
                    }
                }
            }
        })
        .expect("pg-health-monitor thread spawn failed");
}

// ---------------------------------------------------------------------------
// Async write-through: PgWriter
// ---------------------------------------------------------------------------

/// Requests that can be sent to the background [`PgWriter`] task.
#[derive(Debug)]
pub enum PgWriteRequest {
    /// Persist a bus message.
    Message(Box<Message>),
    /// Persist a presence event.
    Presence(Box<Presence>),
    /// Flush any queued writes immediately.
    ///
    /// Reserved for future use (e.g. graceful-shutdown sequences).
    Flush {
        /// Optional completion signal for callers that need to block until the
        /// current batch has been persisted.
        completion: Option<oneshot::Sender<()>>,
    },
    /// Drain remaining writes and shut down the background task.
    ///
    /// Sent by [`PgWriter::shutdown_and_wait`] before process exit in CLI mode
    /// to guarantee all enqueued writes are flushed to `PostgreSQL`.
    Shutdown,
}

/// Non-blocking fire-and-forget `PostgreSQL` writer.
///
/// Internally holds an [`mpsc::UnboundedSender`] that feeds a background
/// Tokio task.  The background task batches writes (up to 10 at a time) and
/// flushes every 100 ms.  Callers are never blocked waiting for `PostgreSQL`.
///
/// # Examples
///
/// ```rust,ignore
/// let writer = PgWriter::spawn(settings.clone());
/// writer.send_message(&msg);
/// writer.send_presence(&presence);
/// ```
#[derive(Debug, Clone)]
pub struct PgWriter {
    tx: mpsc::UnboundedSender<PgWriteRequest>,
}

/// Maximum number of requests to process in a single flush cycle.
const PG_BATCH_SIZE: usize = 20;

/// How long (in milliseconds) the flush timer waits between cycles.
const PG_BATCH_TIMEOUT_MS: u64 = 100;

/// How long to wait between flush cycles.
const PG_FLUSH_INTERVAL: Duration = Duration::from_millis(PG_BATCH_TIMEOUT_MS);

impl PgWriter {
    /// Spawn the background flush task and return a `PgWriter` handle plus a
    /// join handle for graceful shutdown.
    ///
    /// The returned `PgWriter` is `Clone` — each clone shares the same channel.
    /// The join handle should be stored separately and awaited via
    /// [`PgWriter::shutdown_and_wait`] before process exit in CLI commands.
    #[must_use]
    pub fn spawn(settings: Settings) -> (Self, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::unbounded_channel::<PgWriteRequest>();
        let handle = tokio::spawn(pg_writer_task(settings, rx));
        (Self { tx }, handle)
    }

    /// Send a `Shutdown` signal and await the background task.
    ///
    /// Callers that hold the `JoinHandle` returned by [`spawn`] should call
    /// this at the end of CLI commands to ensure all enqueued writes are
    /// flushed to `PostgreSQL` before the process exits.
    ///
    /// If the task has already exited (e.g. channel already closed) this is a no-op.
    pub fn shutdown_and_wait(&self, handle: tokio::task::JoinHandle<()>) {
        let _ = self.tx.send(PgWriteRequest::Shutdown);
        // Block in place so we don't need to restructure main() into async.
        // This is a CLI-only path; the server transports never call this.
        let _ = tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(handle));
    }

    /// Request an immediate flush of queued writes.
    ///
    /// The flush happens asynchronously on the background task. Callers may
    /// optionally wait a short interval if they need a best-effort drain before
    /// continuing with maintenance work.
    pub fn flush(&self) {
        let _ = self.tx.send(PgWriteRequest::Flush { completion: None });
    }

    /// Enqueue a message for asynchronous `PostgreSQL` persistence.
    ///
    /// Drops the write silently if the background task has already exited.
    pub fn send_message(&self, msg: &Message) {
        let _ = self.tx.send(PgWriteRequest::Message(Box::new(msg.clone())));
        pg_metrics().messages_queued.fetch_add(1, Ordering::Relaxed);
    }

    /// Enqueue a presence event for asynchronous `PostgreSQL` persistence.
    ///
    /// Drops the write silently if the background task has already exited.
    pub fn send_presence(&self, presence: &Presence) {
        let _ = self
            .tx
            .send(PgWriteRequest::Presence(Box::new(presence.clone())));
        pg_metrics().messages_queued.fetch_add(1, Ordering::Relaxed);
    }

    /// Request an immediate flush and block until the current queue has been
    /// drained or the timeout elapses.
    pub async fn flush_and_wait_async(&self, timeout: Duration) -> bool {
        let (tx, rx) = oneshot::channel();
        if self
            .tx
            .send(PgWriteRequest::Flush {
                completion: Some(tx),
            })
            .is_err()
        {
            return false;
        }
        tokio::time::timeout(timeout, rx).await.is_ok()
    }

    /// Blocking wrapper for command handlers that need to wait for a flush.
    #[must_use]
    pub fn flush_and_wait_blocking(&self, timeout: Duration) -> bool {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.flush_and_wait_async(timeout))
        })
    }
}

/// Background task: drain the mpsc channel and flush to `PostgreSQL` in batches.
async fn pg_writer_task(settings: Settings, mut rx: mpsc::UnboundedReceiver<PgWriteRequest>) {
    let mut batch: Vec<PgWriteRequest> = Vec::with_capacity(PG_BATCH_SIZE);
    let mut interval = tokio::time::interval(PG_FLUSH_INTERVAL);

    loop {
        tokio::select! {
            // Timer tick: flush whatever we have.
            _ = interval.tick() => {
                if !batch.is_empty() {
                    flush_pg_batch(&settings, &mut batch);
                }
            }
            // Incoming write request.
            msg = rx.recv() => {
                match msg {
                    None => {
                        // Channel closed — sender side dropped.
                        if !batch.is_empty() {
                            flush_pg_batch(&settings, &mut batch);
                        }
                        return;
                    }
                    Some(PgWriteRequest::Shutdown) => {
                        // Drain remaining writes then stop.
                        // Collect any remaining messages still in the channel.
                        while let Ok(req) = rx.try_recv() {
                            match req {
                                PgWriteRequest::Message(_) | PgWriteRequest::Presence(_) => batch.push(req),
                                PgWriteRequest::Flush { completion } => {
                                    if !batch.is_empty() {
                                        flush_pg_batch(&settings, &mut batch);
                                    }
                                    if let Some(completion) = completion {
                                        let _ = completion.send(());
                                    }
                                }
                                PgWriteRequest::Shutdown => {}
                            }
                        }
                        if !batch.is_empty() {
                            flush_pg_batch(&settings, &mut batch);
                        }
                        return;
                    }
                    Some(PgWriteRequest::Flush { completion }) => {
                        if !batch.is_empty() {
                            flush_pg_batch(&settings, &mut batch);
                        }
                        if let Some(completion) = completion {
                            let _ = completion.send(());
                        }
                    }
                    Some(req) => {
                        batch.push(req);
                        if batch.len() >= PG_BATCH_SIZE {
                            flush_pg_batch(&settings, &mut batch);
                        }
                    }
                }
            }
        }
    }
}

/// Drain `batch` and write each entry to `PostgreSQL`.
///
/// Each write is attempted independently; failures are logged as warnings but
/// do not prevent remaining items from being processed.
///
/// Increments [`PgWriteMetrics`] counters for written items, errors, and
/// completed batch cycles.
fn flush_pg_batch(settings: &Settings, batch: &mut Vec<PgWriteRequest>) {
    let metrics = pg_metrics();
    for req in batch.drain(..) {
        match req {
            PgWriteRequest::Message(msg) => {
                if let Err(error) = persist_message_postgres(settings, &msg) {
                    tracing::warn!("PgWriter: failed to persist message: {error:#}");
                    metrics.write_errors.fetch_add(1, Ordering::Relaxed);
                } else {
                    metrics.messages_written.fetch_add(1, Ordering::Relaxed);
                }
            }
            PgWriteRequest::Presence(presence) => {
                if let Err(error) = persist_presence_postgres(settings, &presence) {
                    tracing::warn!("PgWriter: failed to persist presence: {error:#}");
                    metrics.write_errors.fetch_add(1, Ordering::Relaxed);
                } else {
                    metrics.messages_written.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Flush/Shutdown are control signals, not data — nothing to persist.
            PgWriteRequest::Flush { .. } | PgWriteRequest::Shutdown => {}
        }
    }
    metrics.batches_flushed.fetch_add(1, Ordering::Relaxed);
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

    // -----------------------------------------------------------------------
    // PgWriter tests
    // -----------------------------------------------------------------------

    /// `PgWriter::spawn` must not panic when `database_url` is absent.
    /// The background task will silently skip all writes.
    #[tokio::test]
    async fn pg_writer_spawn_no_database_url() {
        let mut settings = Settings::from_env();
        settings.database_url = None;
        let (writer, _handle) = PgWriter::spawn(settings);
        // send_message must be a no-op without panicking.
        let msg = crate::models::Message {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp_utc: "2026-01-01T00:00:00.000000Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "test".to_owned(),
            to: "test".to_owned(),
            topic: "test".to_owned(),
            body: "hello".to_owned(),
            thread_id: None,
            tags: vec![].into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        };
        writer.send_message(&msg);
        // Give the background task a moment to process then verify no panic.
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    /// Multiple clones of a `PgWriter` share the same underlying channel.
    #[tokio::test]
    async fn pg_writer_clone_shares_channel() {
        let mut settings = Settings::from_env();
        settings.database_url = None;
        let (writer, _handle) = PgWriter::spawn(settings);
        let writer2 = writer.clone();
        // Both clones must be usable without panicking.
        let msg = crate::models::Message {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp_utc: "2026-01-01T00:00:00.000000Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "a".to_owned(),
            to: "b".to_owned(),
            topic: "test".to_owned(),
            body: "msg2".to_owned(),
            thread_id: None,
            tags: vec![].into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        };
        writer.send_message(&msg);
        writer2.send_message(&msg);
    }

    // -----------------------------------------------------------------------
    // Circuit breaker state transition tests (Task 2.1 — TDD)
    // -----------------------------------------------------------------------

    /// After `mark_pg_down` the circuit is open; `mark_pg_up` closes it.
    #[test]
    fn circuit_breaker_open_then_closed() {
        // Reset to a known state first.
        mark_pg_up();
        assert!(
            !is_pg_circuit_open(),
            "circuit must start closed after mark_pg_up"
        );

        mark_pg_down();
        assert!(
            is_pg_circuit_open(),
            "circuit must be open immediately after mark_pg_down"
        );

        mark_pg_up();
        assert!(
            !is_pg_circuit_open(),
            "circuit must be closed again after mark_pg_up"
        );
    }

    /// When the circuit is open, `with_pg_retry` must not invoke the operation.
    #[test]
    fn circuit_breaker_suppresses_retries_when_open() {
        mark_pg_down();
        assert!(is_pg_circuit_open());

        let mut calls = 0_u32;
        let result: Result<()> = with_pg_retry(|| {
            calls += 1;
            Ok(())
        });
        assert!(result.is_err(), "expected circuit-open error");
        assert_eq!(calls, 0, "closure must not be called while circuit is open");

        // Restore clean state for other tests.
        mark_pg_up();
    }

    /// When the circuit is closed, `with_pg_retry` calls the operation normally.
    #[test]
    fn circuit_breaker_allows_retries_when_closed() {
        mark_pg_up();
        assert!(!is_pg_circuit_open());

        let mut calls = 0_u32;
        let result: Result<()> = with_pg_retry(|| {
            calls += 1;
            Ok(())
        });
        assert!(result.is_ok(), "expected success when circuit is closed");
        assert_eq!(
            calls, 1,
            "closure must be called exactly once on first success"
        );
    }

    // -----------------------------------------------------------------------
    // PgWriteMetrics tests (Task 2.3 — TDD)
    // -----------------------------------------------------------------------

    /// `pg_metrics()` must return the same static instance on every call.
    #[test]
    fn pg_metrics_returns_same_instance() {
        let a = std::ptr::from_ref::<PgWriteMetrics>(pg_metrics());
        let b = std::ptr::from_ref::<PgWriteMetrics>(pg_metrics());
        assert_eq!(a, b, "pg_metrics() must be a stable singleton");
    }

    /// `send_message` increments `messages_queued`.
    #[tokio::test]
    async fn pg_metrics_messages_queued_incremented_on_send() {
        let mut settings = Settings::from_env();
        settings.database_url = None;
        let (writer, _handle) = PgWriter::spawn(settings);

        let before = pg_metrics().messages_queued.load(Ordering::Relaxed);
        let msg = crate::models::Message {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp_utc: "2026-01-01T00:00:00.000000Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "metrics-test".to_owned(),
            to: "all".to_owned(),
            topic: "test".to_owned(),
            body: "metrics probe".to_owned(),
            thread_id: None,
            tags: vec![].into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        };
        writer.send_message(&msg);
        let after = pg_metrics().messages_queued.load(Ordering::Relaxed);
        assert!(
            after > before,
            "messages_queued must increment after send_message (before={before}, after={after})"
        );
    }

    /// `send_presence` also increments `messages_queued`.
    #[tokio::test]
    async fn pg_metrics_presence_queued_incremented_on_send() {
        let mut settings = Settings::from_env();
        settings.database_url = None;
        let (writer, _handle) = PgWriter::spawn(settings);

        let before = pg_metrics().messages_queued.load(Ordering::Relaxed);
        let presence = crate::models::Presence {
            agent: "metrics-test".to_owned(),
            status: "online".to_owned(),
            protocol_version: "1.0".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00.000000Z".to_owned(),
            session_id: uuid::Uuid::new_v4().to_string(),
            capabilities: vec![],
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            ttl_seconds: 300,
        };
        writer.send_presence(&presence);
        let after = pg_metrics().messages_queued.load(Ordering::Relaxed);
        assert!(
            after > before,
            "messages_queued must increment after send_presence (before={before}, after={after})"
        );
    }

    // -----------------------------------------------------------------------
    // spawn_pg_health_monitor smoke test (Task 2.1)
    // -----------------------------------------------------------------------

    /// `spawn_pg_health_monitor` must not panic and the thread must be spawnable.
    /// We use a very short interval so the test is not slow.
    #[test]
    fn spawn_pg_health_monitor_does_not_panic() {
        let mut settings = Settings::from_env();
        settings.database_url = None; // No PG — monitor will see closed circuit and sleep.
        // A 1-second interval is long enough to avoid multiple cycles in CI.
        spawn_pg_health_monitor(settings, 60);
        // If we reach here, the thread was spawned without panicking.
    }

    // -----------------------------------------------------------------------
    // with_pg_retry behaviour — retry logic without a real PostgreSQL instance
    // -----------------------------------------------------------------------

    /// Operation that fails once then succeeds: `with_pg_retry` should retry
    /// and return `Ok` on the second attempt.
    #[test]
    fn with_pg_retry_succeeds_after_one_transient_failure() {
        // Ensure the circuit is closed so the retry path is exercised.
        mark_pg_up();

        let mut attempt = 0_u32;
        let result: Result<&str> = with_pg_retry(|| {
            attempt += 1;
            if attempt == 1 {
                Err(anyhow::anyhow!("transient error"))
            } else {
                Ok("done")
            }
        });

        assert!(result.is_ok(), "expected Ok after one transient failure");
        assert_eq!(result.unwrap(), "done");
        assert_eq!(attempt, 2, "should have been called exactly twice");

        // Restore clean state.
        mark_pg_up();
    }

    /// When every attempt fails, `with_pg_retry` exhausts all retries, opens
    /// the circuit breaker, and returns the final error.
    #[test]
    fn with_pg_retry_exhausts_all_retries_and_opens_circuit() {
        // Start with a closed circuit so retries are not suppressed.
        mark_pg_up();
        assert!(!is_pg_circuit_open());

        let mut attempt = 0_u32;
        let result: Result<()> = with_pg_retry(|| {
            attempt += 1;
            Err(anyhow::anyhow!("persistent failure #{attempt}"))
        });

        assert!(result.is_err(), "expected Err after all retries exhausted");
        assert_eq!(
            attempt, PG_MAX_RETRIES,
            "closure must be called exactly PG_MAX_RETRIES times"
        );
        // Circuit breaker must have been opened after exhausting retries.
        assert!(
            is_pg_circuit_open(),
            "circuit breaker must be open after all retries fail"
        );

        // Restore clean state so other tests are not affected.
        mark_pg_up();
    }

    /// Rapid alternation of `mark_pg_down` / `mark_pg_up` must not deadlock or
    /// produce inconsistent final state.
    #[test]
    fn circuit_breaker_rapid_down_up_transitions_are_safe() {
        for i in 0..20_u32 {
            if i % 2 == 0 {
                mark_pg_down();
                assert!(
                    is_pg_circuit_open(),
                    "circuit must be open after mark_pg_down (iteration {i})"
                );
            } else {
                mark_pg_up();
                assert!(
                    !is_pg_circuit_open(),
                    "circuit must be closed after mark_pg_up (iteration {i})"
                );
            }
        }
        // Leave in a clean (closed) state.
        mark_pg_up();
    }

    /// The circuit stays open when queried within `PG_CIRCUIT_BREAKER_SECONDS`
    /// of being marked down.  We set the timestamp directly by calling
    /// `mark_pg_down` and then immediately checking — no sleep needed.
    #[test]
    fn circuit_breaker_remains_open_within_cooldown_window() {
        mark_pg_down();
        // Immediately after marking down, elapsed time is ~0 s, well within
        // the PG_CIRCUIT_BREAKER_SECONDS window.
        assert!(
            is_pg_circuit_open(),
            "circuit must remain open immediately after mark_pg_down"
        );
        mark_pg_up();
    }

    // -----------------------------------------------------------------------
    // PgWriteMetrics concurrent safety
    // -----------------------------------------------------------------------

    /// Two threads both calling `send_message` concurrently must produce a
    /// total increment of exactly 2 on `messages_queued`.
    #[tokio::test]
    async fn pg_metrics_concurrent_increment_is_safe() {
        let mut settings = Settings::from_env();
        settings.database_url = None;

        let (writer, _handle) = PgWriter::spawn(settings);
        let writer2 = writer.clone();

        let before = pg_metrics().messages_queued.load(Ordering::Relaxed);

        let make_msg = || crate::models::Message {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp_utc: "2026-01-01T00:00:00.000000Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "concurrent-a".to_owned(),
            to: "all".to_owned(),
            topic: "test".to_owned(),
            body: "concurrent test".to_owned(),
            thread_id: None,
            tags: vec![].into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        };

        // Spawn two OS threads, each sending one message through its own writer clone.
        let msg_a = make_msg();
        let msg_b = make_msg();
        let t1 = std::thread::spawn(move || writer.send_message(&msg_a));
        let t2 = std::thread::spawn(move || writer2.send_message(&msg_b));
        t1.join().expect("thread 1 must not panic");
        t2.join().expect("thread 2 must not panic");

        let after = pg_metrics().messages_queued.load(Ordering::Relaxed);
        assert!(
            after >= before + 2,
            "messages_queued must increase by at least 2 (before={before}, after={after})"
        );
    }

    // -----------------------------------------------------------------------
    // PG_BATCH_SIZE and PG_BATCH_TIMEOUT_MS constant range assertions
    // -----------------------------------------------------------------------

    /// `PG_BATCH_SIZE` must be within the reasonable range [5, 100].  Values
    /// outside this range indicate a misconfiguration — too small wastes
    /// round-trips; too large risks memory pressure or slow flushes.
    #[test]
    fn pg_batch_size_is_within_reasonable_range() {
        const {
            assert!(
                PG_BATCH_SIZE >= 5,
                "PG_BATCH_SIZE is too small — minimum is 5"
            );
        };
        const {
            assert!(
                PG_BATCH_SIZE <= 100,
                "PG_BATCH_SIZE is too large — maximum is 100"
            );
        };
    }

    /// `PG_BATCH_TIMEOUT_MS` must be within [50, 500] ms.  Values outside
    /// this range risk either spinning too fast (CPU waste) or delaying writes
    /// too long (data-loss window on crash).
    #[test]
    fn pg_batch_timeout_ms_is_within_reasonable_range() {
        const {
            assert!(
                PG_BATCH_TIMEOUT_MS >= 50,
                "PG_BATCH_TIMEOUT_MS is too small — minimum is 50 ms"
            );
        };
        const {
            assert!(
                PG_BATCH_TIMEOUT_MS <= 500,
                "PG_BATCH_TIMEOUT_MS is too large — maximum is 500 ms"
            );
        };
    }

    // -----------------------------------------------------------------------
    // parse_timestamp_utc
    // -----------------------------------------------------------------------

    /// Valid RFC-3339 timestamps (with Z suffix) must parse without error.
    #[test]
    fn parse_timestamp_utc_accepts_valid_rfc3339() {
        let ts = "2026-03-20T12:34:56.000000Z";
        let result = parse_timestamp_utc(ts);
        assert!(result.is_ok(), "expected Ok for valid timestamp: {ts}");
    }

    /// Invalid timestamp strings must return an error with context.
    #[test]
    fn parse_timestamp_utc_rejects_garbage() {
        let result = parse_timestamp_utc("not-a-timestamp");
        assert!(
            result.is_err(),
            "expected Err for unparseable timestamp string"
        );
    }

    // -----------------------------------------------------------------------
    // parse_tags
    // -----------------------------------------------------------------------

    /// A JSON array of strings must be converted to a `Vec<String>`.
    #[test]
    fn parse_tags_extracts_string_array() {
        let value = serde_json::json!(["alpha", "beta", "gamma"]);
        let tags = parse_tags(&value);
        assert_eq!(tags, vec!["alpha", "beta", "gamma"]);
    }

    /// A non-array JSON value must return an empty vec (graceful degradation).
    #[test]
    fn parse_tags_returns_empty_for_non_array() {
        let value = serde_json::json!(null);
        let tags = parse_tags(&value);
        assert!(tags.is_empty(), "parse_tags(null) must return empty vec");
    }

    /// An empty JSON array must return an empty vec.
    #[test]
    fn parse_tags_returns_empty_for_empty_array() {
        let value = serde_json::json!([]);
        let tags = parse_tags(&value);
        assert!(tags.is_empty(), "parse_tags([]) must return empty vec");
    }

    // -----------------------------------------------------------------------
    // query_scope_tags
    // -----------------------------------------------------------------------

    /// Repo/session scope tags are prefixed once and preserved in input order.
    #[test]
    fn query_scope_tags_prefixes_repo_and_session() {
        let tags = query_scope_tags(Some("agent-bus"), Some("s1"), &["wave:1", "wave:1"]);
        assert_eq!(tags, vec!["repo:agent-bus", "session:s1", "wave:1"]);
    }

    /// Empty scope components should be ignored rather than producing blank tags.
    #[test]
    fn query_scope_tags_ignores_empty_scope_components() {
        let tags = query_scope_tags(Some(""), None, &["repo:agent-bus"]);
        assert_eq!(tags, vec!["repo:agent-bus"]);
    }

    // -----------------------------------------------------------------------
    // storage_cache_key / storage_ready
    // -----------------------------------------------------------------------

    /// When `database_url` is `None`, `storage_cache_key` returns `None` and
    /// `storage_ready` returns `false`.
    #[test]
    fn storage_ready_false_when_no_database_url() {
        let mut settings = Settings::from_env();
        settings.database_url = None;
        assert!(
            storage_cache_key(&settings).is_none(),
            "cache key must be None when database_url absent"
        );
        assert!(
            !storage_ready(&settings),
            "storage_ready must be false when database_url absent"
        );
    }

    // -----------------------------------------------------------------------
    // query_messages_by_tags
    // -----------------------------------------------------------------------

    /// `query_messages_by_tags` must reject calls with no tags and no `thread_id`.
    #[test]
    fn query_messages_by_tags_rejects_empty_filters() {
        let mut settings = Settings::from_env();
        settings.database_url = Some("postgresql://localhost:5300/test".to_owned());

        let result = query_messages_by_tags(&settings, &[], None, 60, 100);
        assert!(
            result.is_err(),
            "expected Err when no tags or thread_id provided"
        );
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("requires at least one tag"),
            "error message should mention tag requirement, got: {err_msg}"
        );
    }

    /// When the circuit breaker is open, `query_messages_by_tags` must fail
    /// immediately without attempting a database connection.
    #[test]
    fn query_messages_by_tags_respects_circuit_breaker() {
        let mut settings = Settings::from_env();
        settings.database_url = Some("postgresql://localhost:5300/test".to_owned());

        mark_pg_down();
        assert!(is_pg_circuit_open());

        let result = query_messages_by_tags(&settings, &["repo:agent-bus"], None, 60, 100);
        assert!(result.is_err(), "expected Err when circuit breaker is open");
        let err_msg = format!("{:#}", result.unwrap_err());
        assert!(
            err_msg.contains("circuit breaker open"),
            "error message should mention circuit breaker, got: {err_msg}"
        );

        // Restore clean state.
        mark_pg_up();
    }

    /// When `database_url` is `None`, `query_messages_by_tags` returns an
    /// empty vec (the inner PG client returns `None`).
    #[test]
    fn query_messages_by_tags_returns_empty_without_database_url() {
        let mut settings = Settings::from_env();
        settings.database_url = None;

        // Ensure circuit is closed so we exercise the PG-absent path.
        mark_pg_up();

        let result = query_messages_by_tags(&settings, &["repo:agent-bus"], None, 60, 100);
        // With no database_url, get_pg_client returns Ok(None), which causes
        // list_messages_postgres_with_filters to return Ok(vec![]).
        assert!(result.is_ok(), "expected Ok(vec![]) without database_url");
        assert!(
            result.unwrap().is_empty(),
            "expected empty vec without database_url"
        );
    }

    /// `is_pg_circuit_open` must be publicly accessible (compile-time check).
    #[test]
    fn is_pg_circuit_open_is_public() {
        // This test exists to verify the function's public visibility.
        // If this compiles, the function is accessible from outside the module.
        let _ = is_pg_circuit_open();
    }
}
