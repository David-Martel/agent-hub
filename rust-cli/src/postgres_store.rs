//! `PostgreSQL` durable storage for messages and presence events.

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

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
    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 0..PG_MAX_RETRIES {
        match operation() {
            Ok(result) => return Ok(result),
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
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("PostgreSQL operation failed after retries")))
}

pub(crate) fn run_postgres_blocking<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::task::block_in_place(operation)
    } else {
        operation()
    }
}

pub(crate) fn connect_postgres(settings: &Settings) -> Result<Option<PgClient>> {
    let Some(database_url) = settings.database_url.as_deref() else {
        return Ok(None);
    };
    let client = PgClient::connect(database_url, NoTls).context("PostgreSQL connection failed")?;
    Ok(Some(client))
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
            let Some(mut client) = connect_postgres(settings)? else {
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
            Ok(())
        })
    })
}

pub(crate) fn persist_presence_postgres(settings: &Settings, presence: &Presence) -> Result<()> {
    with_pg_retry(|| {
        run_postgres_blocking(|| {
            let Some(mut client) = connect_postgres(settings)? else {
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
            Ok(())
        })
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
        let Some(mut client) = connect_postgres(settings)? else {
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
        Ok(messages)
    })
}

pub(crate) fn count_messages_postgres(settings: &Settings) -> Option<i64> {
    run_postgres_blocking(|| {
        let Some(mut client) = connect_postgres(settings)? else {
            return Ok(None);
        };
        let row = client.query_one(
            &format!("select count(*) as cnt from {}", settings.message_table),
            &[],
        )?;
        Ok(Some(row.get::<_, i64>("cnt")))
    })
    .ok()
    .flatten()
}

pub(crate) fn count_presence_postgres(settings: &Settings) -> Option<i64> {
    run_postgres_blocking(|| {
        let Some(mut client) = connect_postgres(settings)? else {
            return Ok(None);
        };
        let row = client.query_one(
            &format!(
                "select count(*) as cnt from {}",
                settings.presence_event_table
            ),
            &[],
        )?;
        Ok(Some(row.get::<_, i64>("cnt")))
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
        let Some(mut client) = connect_postgres(settings)? else {
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
        let Some(mut client) = connect_postgres(settings)? else {
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
        let Some(mut client) = connect_postgres(settings)? else {
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
        Ok(rows.iter().map(row_to_presence).collect())
    })
}

pub(crate) fn probe_postgres(settings: &Settings) -> (Option<bool>, Option<String>, bool) {
    if settings.database_url.is_none() {
        return (None, None, false);
    }
    match run_postgres_blocking(|| {
        let Some(mut client) = connect_postgres(settings)? else {
            return Ok((None, None, false));
        };
        ensure_postgres_storage(&mut client, settings)?;
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
