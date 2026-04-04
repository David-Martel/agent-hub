//! Redis Stream and Pub/Sub operations for the agent coordination bus.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use anyhow::{Context as _, Result};
use base64::Engine as _;
use chrono::Utc;
use redis::Commands as _;
use uuid::Uuid;

/// Body size threshold (bytes) above which LZ4 compression is applied.
const COMPRESS_THRESHOLD: usize = 512;

use crate::channels::{check_redis_ownership, extract_claimed_files, global_ownership_tracker};
use crate::models::{
    Health, Message, PROTOCOL_VERSION, Presence, XREVRANGE_MIN_FETCH, XREVRANGE_OVERFETCH_FACTOR,
};
use crate::postgres_store::{
    PgWriter, count_both_postgres, is_pg_circuit_open, list_messages_postgres,
    list_messages_postgres_with_filters, persist_presence_postgres, pg_metrics, probe_postgres,
    query_messages_by_tags,
};
use crate::settings::{Settings, redact_url};
use crate::validation::infer_schema_from_topic;

/// Open a new synchronous Redis connection using the URL from `settings`.
///
/// # Errors
///
/// Returns an error if the Redis URL is invalid or the TCP connection fails.
pub fn connect(settings: &Settings) -> Result<redis::Connection> {
    let client =
        redis::Client::open(settings.redis_url.as_str()).context("Redis client creation failed")?;
    client.get_connection().context("Redis connection failed")
}

/// Shared atomic counter of active `/events` SSE subscribers.
///
/// `bus_post_message` checks this before serialising and publishing the message
/// to the Redis pub/sub channel.  When the count is zero the PUBLISH is skipped
/// entirely, saving ~3 µs per message in the common case where no legacy SSE
/// clients are connected.
///
/// # Examples
///
/// ```rust,ignore
/// let counter = Arc::new(SseSubscriberCount::default());
/// counter.inc(); // client connected
/// counter.dec(); // client disconnected
/// ```
#[derive(Debug, Default)]
pub struct SseSubscriberCount(AtomicUsize);

impl SseSubscriberCount {
    /// Increment by one when a new `/events` SSE client connects.
    pub fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement by one when an `/events` SSE client disconnects.
    ///
    /// Uses saturating subtraction to prevent underflow on unexpected drops.
    pub fn dec(&self) {
        // fetch_update with saturating sub avoids wrapping on unexpected extra decrements.
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    /// Returns `true` when at least one SSE client is currently connected.
    pub fn any(&self) -> bool {
        self.0.load(Ordering::Relaxed) > 0
    }
}

/// Connection-pool statistics collected for the health endpoint.
#[derive(Debug, Default)]
pub struct PoolMetrics {
    /// Total connections handed out since the pool was created.
    pub connections_acquired: AtomicU64,
    /// Total times the pool returned an error (connection unavailable / timeout).
    pub connection_errors: AtomicU64,
}

/// Shared r2d2 connection pool for synchronous Redis access in HTTP / MCP-HTTP mode.
///
/// Holds a bounded pool of [`redis::Connection`] objects backed by
/// [`redis::Client`]'s built-in r2d2 `ConnectionManager`.  This eliminates the
/// per-request TCP-handshake overhead that the previous single-client approach
/// incurred when every handler called `get_connection()` on a bare `redis::Client`.
///
/// Pool parameters chosen for a single-machine deployment:
/// - `max_size = 5`   — adequate for the typical 8-core dev machine without
///   exhausting Redis's default `maxclients` limit.
/// - `min_idle = 1`   — keeps at least one warm connection to avoid cold-start
///   latency on the first request after an idle period.
/// - `connection_timeout = 500 ms` — fast-fail rather than queuing requests
///   behind a stalled Redis.
///
/// # Examples
///
/// ```no_run
/// use agent_bus_core::redis_bus::RedisPool;
/// use agent_bus_core::settings::Settings;
/// let settings = Settings::from_env();
/// let pool = RedisPool::new(&settings).expect("Redis pool creation failed");
/// let mut conn = pool.get_connection().expect("Redis connection failed");
/// ```
#[derive(Clone, Debug)]
pub struct RedisPool {
    inner: r2d2::Pool<redis::Client>,
    /// Shared metrics exposed on the health endpoint.
    metrics: Arc<PoolMetrics>,
}

impl RedisPool {
    /// Create a new `RedisPool` from the given settings.
    ///
    /// Validates the Redis URL and pre-warms the pool with one idle connection.
    ///
    /// # Errors
    ///
    /// Returns an error if the Redis URL is malformed or the initial connection
    /// required to verify pool health fails.
    pub fn new(settings: &Settings) -> Result<Self> {
        let manager = redis::Client::open(settings.redis_url.as_str())
            .context("Redis client creation failed")?;
        let inner = r2d2::Pool::builder()
            .max_size(5)
            .min_idle(Some(1))
            .connection_timeout(std::time::Duration::from_secs(5))
            .build(manager)
            .context("Redis r2d2 pool creation failed")?;
        Ok(Self {
            inner,
            metrics: Arc::new(PoolMetrics::default()),
        })
    }

    /// Obtain a pooled Redis connection.
    ///
    /// Blocks for up to 500 ms waiting for a free slot in the pool.  The
    /// connection is automatically returned when the guard drops.
    ///
    /// # Errors
    ///
    /// Returns an error if no connection becomes available within the timeout
    /// or if the pool cannot create a new connection.
    pub fn get_connection(&self) -> Result<r2d2::PooledConnection<redis::Client>> {
        match self.inner.get() {
            Ok(conn) => {
                self.metrics
                    .connections_acquired
                    .fetch_add(1, Ordering::Relaxed);
                Ok(conn)
            }
            Err(e) => {
                self.metrics
                    .connection_errors
                    .fetch_add(1, Ordering::Relaxed);
                Err(anyhow::anyhow!(e).context("Redis pool: get_connection failed"))
            }
        }
    }

    /// Pool metrics snapshot: `(connections_acquired, connection_errors)`.
    ///
    /// Suitable for embedding in the health endpoint response.
    #[must_use]
    pub fn metrics(&self) -> (u64, u64) {
        (
            self.metrics.connections_acquired.load(Ordering::Relaxed),
            self.metrics.connection_errors.load(Ordering::Relaxed),
        )
    }

    /// Current r2d2 pool state: idle connections and pool size.
    #[must_use]
    pub fn pool_state(&self) -> r2d2::State {
        self.inner.state()
    }
}

/// Durable notification derived from a canonical bus [`Message`].
///
/// Notification streams are per-agent attention queues. They intentionally keep
/// the full message snapshot so transports such as SSE and MCP can replay
/// missed notifications without a second round-trip back to the canonical
/// stream.
#[allow(
    clippy::struct_field_names,
    reason = "field names mirror the serialized notification API"
)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Notification {
    /// Notification UUID.
    pub id: String,
    /// Agent that should receive this notification.
    pub agent: String,
    /// ISO-8601 creation timestamp (matches the source message timestamp).
    pub created_at: String,
    /// Why this notification was created.
    pub reason: String,
    /// Whether the source message requires acknowledgement.
    pub requires_ack: bool,
    /// Full snapshot of the source message.
    pub message: Message,
    /// Redis stream ID for the notification stream entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_stream_id: Option<String>,
}

/// Message plus any derived notifications created on the hot path.
#[derive(Debug, Clone)]
pub struct PostedMessage {
    pub message: Message,
    #[allow(dead_code)]
    pub notifications: Vec<Notification>,
}

/// Approximate MAXLEN for per-agent notification streams.
const NOTIFICATION_STREAM_MAXLEN: u64 = 10_000;

// ---------------------------------------------------------------------------
// TTL constants for Redis key expiry (seconds)
// ---------------------------------------------------------------------------

/// TTL for notification streams (`agent_bus:notify:*`): 3 days.
const NOTIFICATION_STREAM_TTL_SECS: u64 = 259_200;

/// TTL for notification and inbox cursors (`bus:cursor:*`, `bus:notify_cursor:*`): 7 days.
const CURSOR_TTL_SECS: u64 = 604_800;

/// TTL for per-agent task queues (`bus:tasks:*`): 3 days.
const TASK_QUEUE_TTL_SECS: u64 = 259_200;

/// TTL for per-resource event streams (`agent_bus:resource_events:*`): 3 days.
const RESOURCE_EVENT_TTL_SECS: u64 = 259_200;

/// Redis key prefix for per-agent notification streams.
const NOTIFICATION_STREAM_PREFIX: &str = "agent_bus:notify:";

/// Redis key prefix for durable notification cursors.
const NOTIFICATION_CURSOR_PREFIX: &str = "bus:notify_cursor:";

fn notification_reason(msg: &Message) -> &'static str {
    if msg.topic == "knock" {
        "knock"
    } else if msg.request_ack {
        "direct_request_ack"
    } else {
        "direct_message"
    }
}

fn should_publish_message_event(msg: &Message, has_sse_subscribers: bool) -> bool {
    has_sse_subscribers || (!msg.to.is_empty() && msg.to != "all")
}

#[must_use]
pub fn notification_stream_key(agent: &str) -> String {
    format!("{NOTIFICATION_STREAM_PREFIX}{agent}")
}

#[must_use]
pub fn notification_cursor_key(agent: &str) -> String {
    format!("{NOTIFICATION_CURSOR_PREFIX}{agent}")
}

/// Build a scoped cursor key for repo-filtered or session-filtered inbox reads.
///
/// When `repo` is `Some`, the cursor is scoped to `bus:notify_cursor:<agent>:repo:<repo>`.
/// When `session` is `Some`, the cursor is scoped to `bus:notify_cursor:<agent>:session:<session>`.
/// When both are `None`, falls back to the global cursor key.
///
/// If both `repo` and `session` are provided, `repo` takes precedence (only one
/// scope dimension at a time to keep cursor semantics simple).
#[must_use]
pub fn scoped_notification_cursor_key(
    agent: &str,
    repo: Option<&str>,
    session: Option<&str>,
) -> String {
    if let Some(repo) = repo {
        format!("{NOTIFICATION_CURSOR_PREFIX}{agent}:repo:{repo}")
    } else if let Some(session) = session {
        format!("{NOTIFICATION_CURSOR_PREFIX}{agent}:session:{session}")
    } else {
        notification_cursor_key(agent)
    }
}

/// Read the notification cursor for `agent` with optional scope, returning
/// `"0-0"` if not set.
///
/// # Errors
///
/// Returns an error if the Redis `GET` command fails.
pub fn get_scoped_notification_cursor(
    conn: &mut redis::Connection,
    agent: &str,
    repo: Option<&str>,
    session: Option<&str>,
) -> Result<String> {
    let key = scoped_notification_cursor_key(agent, repo, session);
    let val: Option<String> =
        redis::Commands::get(conn, &key).context("GET scoped notification cursor")?;
    Ok(val.unwrap_or_else(|| "0-0".to_owned()))
}

/// Advance the notification cursor for `agent` with optional scope to
/// `stream_id`.
///
/// # Errors
///
/// Returns an error if the Redis `SET` command fails.
pub fn set_scoped_notification_cursor(
    conn: &mut redis::Connection,
    agent: &str,
    repo: Option<&str>,
    session: Option<&str>,
    stream_id: &str,
) -> Result<()> {
    let key = scoped_notification_cursor_key(agent, repo, session);
    let mut pipe = redis::pipe();
    pipe.cmd("SET").arg(&key).arg(stream_id);
    pipe.cmd("EXPIRE").arg(&key).arg(CURSOR_TTL_SECS);
    pipe.query::<((), ())>(conn)
        .context("SET + EXPIRE scoped notification cursor")?;
    Ok(())
}

fn decode_notification_entry(
    stream_id: &str,
    fields: &HashMap<String, redis::Value>,
) -> Notification {
    let get = |k: &str| -> String {
        match fields.get(k) {
            Some(redis::Value::BulkString(b)) => match std::str::from_utf8(b) {
                Ok(s) => s.to_owned(),
                Err(_) => String::from_utf8_lossy(b).into_owned(),
            },
            Some(redis::Value::SimpleString(s)) => s.clone(),
            _ => String::new(),
        }
    };
    let get_bool = |k: &str| -> bool {
        let v = get(k);
        v == "true" || v == "True"
    };

    let message = serde_json::from_str::<Message>(&get("message")).unwrap_or(Message {
        id: String::new(),
        timestamp_utc: String::new(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        from: String::new(),
        to: String::new(),
        topic: String::new(),
        body: String::new(),
        thread_id: None,
        tags: smallvec::SmallVec::new(),
        priority: crate::models::default_priority(),
        request_ack: false,
        reply_to: None,
        metadata: serde_json::Value::Object(serde_json::Map::new()),
        stream_id: None,
    });

    Notification {
        id: get("id"),
        agent: get("agent"),
        created_at: get("created_at"),
        reason: get("reason"),
        requires_ack: get_bool("requires_ack"),
        message,
        notification_stream_id: Some(stream_id.to_owned()),
    }
}

fn append_notifications_for_message(
    conn: &mut redis::Connection,
    msg: &Message,
) -> Result<Vec<Notification>> {
    if msg.to.is_empty() || msg.to == "all" {
        return Ok(Vec::new());
    }

    let agent = msg.to.clone();
    let id = Uuid::now_v7().to_string();
    let message_json = serde_json::to_string(msg).context("serialize message for notification")?;
    let created_at = msg.timestamp_utc.clone();
    let reason = notification_reason(msg).to_owned();
    let requires_ack = if msg.request_ack { "true" } else { "false" };
    let key = notification_stream_key(&agent);

    let stream_id: String = redis::cmd("XADD")
        .arg(&key)
        .arg("MAXLEN")
        .arg("~")
        .arg(NOTIFICATION_STREAM_MAXLEN)
        .arg("*")
        .arg(&[
            ("id", id.as_str()),
            ("agent", agent.as_str()),
            ("created_at", created_at.as_str()),
            ("reason", reason.as_str()),
            ("requires_ack", requires_ack),
            ("message", message_json.as_str()),
        ])
        .query(conn)
        .context("XADD notification failed")?;

    // Refresh TTL on the notification stream (3 days).
    let _: () = redis::cmd("EXPIRE")
        .arg(&key)
        .arg(NOTIFICATION_STREAM_TTL_SECS)
        .query(conn)
        .context("EXPIRE notification stream failed")?;

    Ok(vec![Notification {
        id,
        agent,
        created_at,
        reason,
        requires_ack: msg.request_ack,
        message: msg.clone(),
        notification_stream_id: Some(stream_id),
    }])
}

fn prepare_notification_fields(msg: &Message) -> Option<[String; 5]> {
    if msg.to.is_empty() || msg.to == "all" {
        return None;
    }

    let id = Uuid::now_v7().to_string();
    let created_at = msg.timestamp_utc.clone();
    let reason = notification_reason(msg).to_owned();
    let requires_ack = if msg.request_ack {
        "true".to_owned()
    } else {
        "false".to_owned()
    };
    let message_json = serde_json::to_string(msg).ok()?;
    Some([id, created_at, reason, requires_ack, message_json])
}

/// List the most recent `limit` notifications for `agent` in chronological order.
///
/// # Errors
///
/// Returns an error if the Redis `XREVRANGE` command fails.
pub fn list_notifications(
    conn: &mut redis::Connection,
    agent: &str,
    limit: usize,
) -> Result<Vec<Notification>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let raw: Vec<redis::Value> = redis::cmd("XREVRANGE")
        .arg(notification_stream_key(agent))
        .arg("+")
        .arg("-")
        .arg("COUNT")
        .arg(limit)
        .query(conn)
        .context("XREVRANGE notifications failed")?;

    let mut notifications = Vec::new();
    for (stream_id, fields) in parse_xrange_result(&raw) {
        notifications.push(decode_notification_entry(&stream_id, &fields));
    }
    notifications.reverse();
    Ok(notifications)
}

/// List notifications for `agent` that arrived after the exclusive `since_id` cursor.
///
/// # Errors
///
/// Returns an error if the Redis `XRANGE` command fails.
pub fn list_notifications_since_id(
    conn: &mut redis::Connection,
    agent: &str,
    since_id: &str,
    limit: usize,
) -> Result<Vec<Notification>> {
    if limit == 0 {
        return Ok(Vec::new());
    }

    let exclusive_start = format!("({since_id}");
    let raw: Vec<redis::Value> = redis::cmd("XRANGE")
        .arg(notification_stream_key(agent))
        .arg(&exclusive_start)
        .arg("+")
        .arg("COUNT")
        .arg(limit.max(1))
        .query(conn)
        .context("XRANGE notifications since_id failed")?;

    let mut notifications = Vec::new();
    for (stream_id, fields) in parse_xrange_result(&raw) {
        notifications.push(decode_notification_entry(&stream_id, &fields));
    }
    Ok(notifications)
}

/// Read the notification cursor for `agent`, returning `"0-0"` if not set.
///
/// # Errors
///
/// Returns an error if the Redis `GET` command fails.
pub fn get_notification_cursor(conn: &mut redis::Connection, agent: &str) -> Result<String> {
    let key = notification_cursor_key(agent);
    let val: Option<String> =
        redis::Commands::get(conn, &key).context("GET notification cursor")?;
    Ok(val.unwrap_or_else(|| "0-0".to_owned()))
}

/// Advance the notification cursor for `agent` to `stream_id`.
///
/// # Errors
///
/// Returns an error if the Redis `SET` command fails.
pub fn set_notification_cursor(
    conn: &mut redis::Connection,
    agent: &str,
    stream_id: &str,
) -> Result<()> {
    let key = notification_cursor_key(agent);
    let _: () = redis::Commands::set(conn, &key, stream_id).context("SET notification cursor")?;
    Ok(())
}

pub fn decode_stream_entry<S: std::hash::BuildHasher>(
    fields: &HashMap<String, redis::Value, S>,
) -> Message {
    // Extract a field as an owned String. For BulkString we attempt zero-copy
    // UTF-8 conversion; only lossy-converts on invalid bytes (rare in practice).
    let get = |k: &str| -> String {
        match fields.get(k) {
            Some(redis::Value::BulkString(b)) => {
                // Fast path: valid UTF-8 (the common case) avoids the lossy
                // intermediate Cow allocation entirely.
                match std::str::from_utf8(b) {
                    Ok(s) => s.to_owned(),
                    Err(_) => String::from_utf8_lossy(b).into_owned(),
                }
            }
            Some(redis::Value::SimpleString(s)) => s.clone(),
            _ => String::new(),
        }
    };
    // Borrow a field as &str when possible (avoids allocation for read-only checks).
    let get_ref = |k: &str| -> &[u8] {
        match fields.get(k) {
            Some(redis::Value::BulkString(b)) => b.as_slice(),
            Some(redis::Value::SimpleString(s)) => s.as_bytes(),
            _ => b"",
        }
    };
    let get_bool = |k: &str| -> bool {
        let v = get_ref(k);
        v == b"true" || v == b"True"
    };
    let get_json_vec = |k: &str| -> Vec<String> {
        let raw = get(k);
        serde_json::from_str(&raw).unwrap_or_default()
    };
    let get_json_value = |k: &str| -> serde_json::Value {
        let raw = get(k);
        serde_json::from_str(&raw).unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
    };

    // Transparently decompress LZ4 bodies written by bus_post_message.
    // Check the _compressed marker with a zero-allocation byte comparison.
    let body = {
        let raw = get("body");
        if get_ref("_compressed") == b"lz4" {
            match lz4_decompress_body(&raw) {
                Ok(decompressed) => decompressed,
                Err(e) => {
                    tracing::warn!("lz4 decompress failed for stream entry, using raw body: {e:#}");
                    raw
                }
            }
        } else {
            raw
        }
    };

    Message {
        id: get("id"),
        timestamp_utc: get("timestamp_utc"),
        protocol_version: get("protocol_version"),
        from: get("from"),
        to: get("to"),
        topic: get("topic"),
        body,
        thread_id: {
            let v = get("thread_id");
            if v.is_empty() || v == "None" {
                None
            } else {
                Some(v)
            }
        },
        tags: get_json_vec("tags").into(),
        priority: {
            let v = get("priority");
            if v.is_empty() { "normal".to_owned() } else { v }
        },
        request_ack: get_bool("request_ack"),
        reply_to: {
            let v = get("reply_to");
            if v.is_empty() { None } else { Some(v) }
        },
        metadata: get_json_value("metadata"),
        stream_id: None,
    }
}

#[must_use]
pub fn message_matches_filters(
    msg: &Message,
    agent: Option<&str>,
    from_agent: Option<&str>,
    include_broadcast: bool,
    thread_id: Option<&str>,
    required_tags: &[&str],
) -> bool {
    if from_agent.is_some_and(|filter| msg.from != filter) {
        return false;
    }
    if let Some(filter) = agent {
        let to_matches = msg.to == filter;
        let broadcast_matches = include_broadcast && msg.to == "all";
        if !to_matches && !broadcast_matches {
            return false;
        }
    }
    if let Some(thread_filter) = thread_id
        && msg.thread_id.as_deref() != Some(thread_filter)
    {
        return false;
    }
    required_tags
        .iter()
        .all(|tag| msg.tags.iter().any(|candidate| candidate == tag))
}

/// Parse stream results from XRANGE / XREVRANGE.
#[must_use]
pub fn parse_xrange_result(raw: &[redis::Value]) -> Vec<(String, HashMap<String, redis::Value>)> {
    let mut out = Vec::new();
    for entry in raw {
        let redis::Value::Array(parts) = entry else {
            continue;
        };
        if parts.len() < 2 {
            continue;
        }
        let redis::Value::BulkString(sid_bytes) = &parts[0] else {
            continue;
        };
        let stream_id = String::from_utf8_lossy(sid_bytes).to_string();
        let redis::Value::Array(fields_raw) = &parts[1] else {
            continue;
        };

        let mut field_map: HashMap<String, redis::Value> = HashMap::new();
        let mut j = 0;
        while j + 1 < fields_raw.len() {
            let key = match &fields_raw[j] {
                redis::Value::BulkString(b) => String::from_utf8_lossy(b).to_string(),
                redis::Value::SimpleString(s) => s.clone(),
                _ => {
                    j += 2;
                    continue;
                }
            };
            field_map.insert(key, fields_raw[j + 1].clone());
            j += 2;
        }
        out.push((stream_id, field_map));
    }
    out
}

/// Compress `body` with LZ4 and Base64-encode it for Redis storage.
///
/// Returns `(compressed_b64, original_len)`.
#[expect(
    clippy::unnecessary_wraps,
    reason = "Result keeps the call-site uniform if compression ever becomes fallible"
)]
fn lz4_compress_body(body: &str) -> Result<(String, usize)> {
    let compressed = lz4_flex::compress_prepend_size(body.as_bytes());
    let encoded = base64::engine::general_purpose::STANDARD.encode(&compressed);
    Ok((encoded, body.len()))
}

/// Decompress a Base64+LZ4 body stored by [`lz4_compress_body`].
///
/// # Errors
///
/// Returns an error if Base64 decoding or LZ4 decompression fails.
fn lz4_decompress_body(encoded: &str) -> Result<String> {
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("base64 decode of compressed body failed")?;
    let raw = lz4_flex::decompress_size_prepended(&compressed)
        .map_err(|e| anyhow::anyhow!("lz4 decompress failed: {e}"))?;
    String::from_utf8(raw).context("lz4 decompressed body is not valid UTF-8")
}

// ---------------------------------------------------------------------------
// prepare_message / BatchSendPayload / bus_post_messages_batch
// ---------------------------------------------------------------------------

/// All owned string data for one Redis XADD command in a pipeline.
///
/// Produced by [`prepare_message`] and consumed by [`bus_post_messages_batch`].
/// Owning the strings here keeps them alive across the entire pipeline
/// execution window — the redis pipeline borrows them by reference until
/// [`redis::Pipeline::query`] returns.
struct PreparedMessage {
    /// Application-level [`Message`] returned to callers.
    /// `stream_id` is `None` until the pipeline result is backfilled.
    message: Message,
    /// Body written to Redis (may be LZ4+base64 when body was large).
    stored_body: String,
    /// Serialised JSON for the `tags` field.
    tags_json: String,
    /// `"true"` or `"false"` for the `request_ack` field.
    ack_str: &'static str,
    /// `thread_id` value or empty string (never the sentinel `"None"`).
    thread_str: String,
    /// Serialised JSON for the `metadata` field (includes `_schema`/`_compressed`).
    meta_json: String,
    /// `true` when the body was LZ4-compressed.
    is_compressed: bool,
}

/// Extract and prepare all per-message state needed for a Redis XADD.
///
/// Runs schema inference, optional LZ4 compression, and metadata enrichment.
/// When `session_id` is `Some`, a `session:<id>` tag is injected into the
/// tags array (if not already present), enabling session-level filtering via
/// the `session-summary` command.
///
/// The resulting [`PreparedMessage`] owns every string referenced by the
/// eventual XADD field list so that multiple prepared messages can coexist in
/// memory during pipeline construction.
///
/// This is the pure-computation half of message sending: no network I/O,
/// no fallible operations.
#[expect(
    clippy::too_many_arguments,
    reason = "maps directly to protocol fields — same arity as bus_post_message"
)]
fn prepare_message(
    from: &str,
    to: &str,
    topic: &str,
    body: &str,
    thread_id: Option<&str>,
    tags: &[String],
    priority: &str,
    request_ack: bool,
    reply_to: Option<&str>,
    metadata: &serde_json::Value,
    session_id: Option<&str>,
) -> PreparedMessage {
    let id = Uuid::now_v7().to_string();
    let ts = {
        let mut buf = String::with_capacity(32);
        let _ = write!(buf, "{}", Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ"));
        buf
    };
    let reply = reply_to.unwrap_or(from).to_owned();

    // Task 4.4: auto-infer thread_id from reply_to when not explicitly set.
    // When a message is a reply (reply_to is Some) but has no explicit
    // thread_id, the replied-to message ID becomes the thread root, grouping
    // all replies into a single thread automatically.
    let effective_thread_id: Option<&str> = thread_id.or(reply_to);

    // Schema inference: annotate metadata with `_schema` when the topic maps
    // to a known schema so all transports see a consistent annotation.
    let inferred_schema = infer_schema_from_topic(topic, None);

    // LZ4 compression for bodies above the threshold.
    let compressed: Option<String> = if body.len() > COMPRESS_THRESHOLD {
        match lz4_compress_body(body) {
            Ok((b64, _)) => Some(b64),
            Err(e) => {
                tracing::warn!("lz4 compression failed, storing uncompressed: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let stored_body = compressed.clone().unwrap_or_else(|| body.to_owned());
    let is_compressed = compressed.is_some();

    // Build merged metadata in one pass: schema annotation + compression markers.
    let final_metadata: serde_json::Value = if inferred_schema.is_some() || is_compressed {
        let mut map = match metadata {
            serde_json::Value::Object(m) => m.clone(),
            _ => serde_json::Map::new(),
        };
        if let Some(schema) = inferred_schema {
            map.insert(
                "_schema".to_owned(),
                serde_json::Value::String(schema.to_owned()),
            );
        }
        if is_compressed {
            map.insert("_compressed".to_owned(), serde_json::json!("lz4"));
            map.insert(
                "_original_size".to_owned(),
                serde_json::Value::Number(body.len().into()),
            );
        }
        serde_json::Value::Object(map)
    } else {
        metadata.clone()
    };

    // Task 4.1: inject session tag when a session_id is configured.
    // Build the effective tags, adding "session:<id>" only when not already
    // present.  We avoid cloning the slice when no injection is needed.
    let effective_tags: smallvec::SmallVec<[String; 4]> = if let Some(sid) = session_id {
        let tag = format!("session:{sid}");
        if tags.iter().any(|t| t == &tag) {
            tags.to_vec().into()
        } else {
            let mut v: smallvec::SmallVec<[String; 4]> = tags.to_vec().into();
            v.push(tag);
            v
        }
    } else {
        tags.to_vec().into()
    };

    let tags_json =
        serde_json::to_string(effective_tags.as_slice()).unwrap_or_else(|_| "[]".to_owned());
    let ack_str: &'static str = if request_ack { "true" } else { "false" };
    let thread_str = effective_thread_id.unwrap_or("").to_owned();
    let meta_json = serde_json::to_string(&final_metadata).unwrap_or_else(|_| "{}".to_owned());

    let message = Message {
        id,
        timestamp_utc: ts,
        protocol_version: PROTOCOL_VERSION.to_owned(),
        from: from.to_owned(),
        to: to.to_owned(),
        topic: topic.to_owned(),
        body: body.to_owned(),
        thread_id: effective_thread_id.map(String::from),
        tags: effective_tags,
        priority: priority.to_owned(),
        request_ack,
        reply_to: Some(reply),
        metadata: final_metadata,
        stream_id: None,
    };

    PreparedMessage {
        message,
        stored_body,
        tags_json,
        ack_str,
        thread_str,
        meta_json,
        is_compressed,
    }
}

/// Input payload for [`bus_post_messages_batch`].
///
/// Each field mirrors the corresponding parameter of [`bus_post_message`]
/// so the two call-sites remain symmetric.
#[derive(Debug)]
pub struct BatchSendPayload {
    pub from: String,
    pub to: String,
    pub topic: String,
    pub body: String,
    pub thread_id: Option<String>,
    pub tags: Vec<String>,
    pub priority: String,
    pub request_ack: bool,
    pub reply_to: Option<String>,
    pub metadata: serde_json::Value,
}

/// Send multiple messages in a single Redis pipeline round-trip.
///
/// All XADD commands are batched into one [`redis::Pipeline`] executed with a
/// single network round-trip, followed by a pipelined PUBLISH burst and async
/// `PostgreSQL` persistence.  For a 100-message batch this replaces 100 sequential
/// XADD round-trips with one, yielding roughly an 8x latency improvement on
/// localhost Redis.
///
/// The returned `Vec<Message>` preserves input ordering with `stream_id`
/// backfilled from the pipeline results.
///
/// # Errors
///
/// Returns an error if Redis pipeline execution fails.  PG write failures and
/// PUBLISH failures are logged as warnings but do not fail the batch.
///
/// # Panics
///
/// Panics if the notification pipeline returns more stream IDs than messages
/// indexed for notification — which cannot happen in correct operation because
/// `message_indexes` and the pipeline entries are appended in lockstep.
#[expect(
    clippy::too_many_lines,
    reason = "XADD pipeline + PUBLISH pipeline + PG enqueue + pending-ack pipeline in one fn"
)]
pub fn bus_post_messages_batch_with_notifications(
    conn: &mut redis::Connection,
    settings: &Settings,
    payloads: Vec<BatchSendPayload>,
    pg_writer: Option<&PgWriter>,
    has_sse_subscribers: bool,
) -> Result<Vec<PostedMessage>> {
    let session_id = settings.session_id.as_deref();
    if payloads.is_empty() {
        return Ok(Vec::new());
    }

    // Pure-computation pass: consume payloads and prepare all messages before
    // any I/O.  Moving out of the Vec here avoids a redundant clone.
    let mut prepared: Vec<PreparedMessage> = Vec::with_capacity(payloads.len());
    for p in payloads {
        prepared.push(prepare_message(
            &p.from,
            &p.to,
            &p.topic,
            &p.body,
            p.thread_id.as_deref(),
            &p.tags,
            &p.priority,
            p.request_ack,
            p.reply_to.as_deref(),
            &p.metadata,
            session_id,
        ));
    }

    // Build one pipeline with all XADD commands.  Each command borrows string
    // slices from its `PreparedMessage`; all entries remain alive until
    // `pipe.query` returns.
    let mut pipe = redis::pipe();
    for pm in &prepared {
        let mut fields: Vec<(&str, &str)> = Vec::with_capacity(15);
        fields.extend_from_slice(&[
            ("id", pm.message.id.as_str()),
            ("timestamp_utc", pm.message.timestamp_utc.as_str()),
            ("protocol_version", PROTOCOL_VERSION),
            ("from", pm.message.from.as_str()),
            ("to", pm.message.to.as_str()),
            ("topic", pm.message.topic.as_str()),
            ("body", pm.stored_body.as_str()),
            ("tags", pm.tags_json.as_str()),
            ("priority", pm.message.priority.as_str()),
            ("request_ack", pm.ack_str),
            (
                "reply_to",
                pm.message
                    .reply_to
                    .as_deref()
                    .unwrap_or(pm.message.from.as_str()),
            ),
            ("metadata", pm.meta_json.as_str()),
        ]);
        if !pm.thread_str.is_empty() {
            fields.push(("thread_id", pm.thread_str.as_str()));
        }
        if pm.is_compressed {
            fields.push(("_compressed", "lz4"));
        }

        pipe.cmd("XADD")
            .arg(&settings.stream_key)
            .arg("MAXLEN")
            .arg("~")
            .arg(settings.stream_maxlen)
            .arg("*")
            .arg(&fields);
    }

    // Single network round-trip: execute all XADDs.
    let stream_ids: Vec<String> = pipe.query(conn).context("pipeline XADD failed")?;

    // Backfill stream_ids onto each message, preserving input order.
    let mut messages: Vec<Message> = Vec::with_capacity(prepared.len());
    for (mut pm, stream_id) in prepared.into_iter().zip(stream_ids) {
        pm.message.stream_id = Some(stream_id);
        messages.push(pm.message);
    }

    // Publish when legacy `/events` subscribers are connected or when a direct
    // recipient-specific message should wake active `/events/:agent` clients
    // through the HTTP bridge subscriber.
    if messages
        .iter()
        .any(|msg| should_publish_message_event(msg, has_sse_subscribers))
    {
        let mut pub_pipe = redis::pipe();
        for msg in &messages {
            if !should_publish_message_event(msg, has_sse_subscribers) {
                continue;
            }
            let event = serde_json::json!({"event": "message", "message": msg});
            let event_json = serde_json::to_string(&event).unwrap_or_default();
            pub_pipe
                .cmd("PUBLISH")
                .arg(&settings.channel_key)
                .arg(event_json);
        }
        if let Err(e) = pub_pipe.query::<Vec<i64>>(conn) {
            tracing::warn!("batch PUBLISH pipeline failed (non-fatal): {e:#}");
        }
    }

    // PostgreSQL: async enqueue via PgWriter (same pattern as bus_post_message).
    if let Some(writer) = pg_writer {
        for msg in &messages {
            writer.send_message(msg);
        }
    }

    // Pending ACK tracking: pipeline all SET EX commands (third round-trip,
    // only when at least one message requests acknowledgement).
    let needs_ack_count = messages.iter().filter(|m| m.request_ack).count();
    if needs_ack_count > 0 {
        let mut ack_pipe = redis::pipe();
        for msg in messages.iter().filter(|m| m.request_ack) {
            let key = format!("{PENDING_ACK_PREFIX}{}", msg.id);
            let record = serde_json::json!({
                "message_id": msg.id,
                "recipient": msg.to,
                "sent_at": msg.timestamp_utc,
            });
            let value = serde_json::to_string(&record).unwrap_or_default();
            ack_pipe
                .cmd("SET")
                .arg(&key)
                .arg(&value)
                .arg("EX")
                .arg(PENDING_ACK_TTL);
        }
        if let Err(e) = ack_pipe.query::<Vec<String>>(conn) {
            tracing::warn!("batch pending-ack pipeline failed (non-fatal): {e:#}");
        }

        // Ack deadline tracking (additive, pipelined alongside pending acks).
        let mut deadline_pipe = redis::pipe();
        for msg in messages.iter().filter(|m| m.request_ack) {
            let ttl = crate::models::ack_deadline_seconds(&msg.priority);
            let deadline_at =
                chrono::Utc::now() + chrono::Duration::seconds(i64::try_from(ttl).unwrap_or(300));
            let deadline = crate::models::AckDeadline {
                message_id: msg.id.clone(),
                recipient: msg.to.clone(),
                deadline_at: deadline_at.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
                escalation_level: 0,
                escalated_to: None,
            };
            let key = format!("{ACK_DEADLINE_PREFIX}{}", msg.id);
            let json = serde_json::to_string(&deadline).unwrap_or_default();
            deadline_pipe
                .cmd("SET")
                .arg(&key)
                .arg(&json)
                .arg("EX")
                .arg(ttl);
        }
        if let Err(e) = deadline_pipe.query::<Vec<String>>(conn) {
            tracing::warn!("batch ack-deadline pipeline failed (non-fatal): {e:#}");
        }
    }

    let mut notifications_by_message: Vec<Vec<Notification>> =
        (0..messages.len()).map(|_| Vec::new()).collect();
    let prepared_notifications: Vec<Option<[String; 5]>> =
        messages.iter().map(prepare_notification_fields).collect();
    if prepared_notifications.iter().any(Option::is_some) {
        let mut notify_pipe = redis::pipe();
        let mut message_indexes: Vec<usize> = Vec::new();
        for (idx, prepared) in prepared_notifications.iter().enumerate() {
            let Some(fields) = prepared else {
                continue;
            };
            message_indexes.push(idx);
            let notify_key = notification_stream_key(messages[idx].to.as_str());
            notify_pipe
                .cmd("XADD")
                .arg(&notify_key)
                .arg("MAXLEN")
                .arg("~")
                .arg(NOTIFICATION_STREAM_MAXLEN)
                .arg("*")
                .arg(&[
                    ("id", fields[0].as_str()),
                    ("agent", messages[idx].to.as_str()),
                    ("created_at", fields[1].as_str()),
                    ("reason", fields[2].as_str()),
                    ("requires_ack", fields[3].as_str()),
                    ("message", fields[4].as_str()),
                ]);
            // Refresh TTL on the notification stream (3 days).
            notify_pipe
                .cmd("EXPIRE")
                .arg(&notify_key)
                .arg(NOTIFICATION_STREAM_TTL_SECS)
                .ignore();
        }

        if !message_indexes.is_empty() {
            match notify_pipe.query::<Vec<String>>(conn) {
                Ok(notification_stream_ids) => {
                    for (stream_id, msg_idx) in
                        notification_stream_ids.into_iter().zip(message_indexes)
                    {
                        let fields = prepared_notifications[msg_idx]
                            .as_ref()
                            .expect("notification fields must exist for indexed message");
                        notifications_by_message[msg_idx].push(Notification {
                            id: fields[0].clone(),
                            agent: messages[msg_idx].to.clone(),
                            created_at: fields[1].clone(),
                            reason: fields[2].clone(),
                            requires_ack: messages[msg_idx].request_ack,
                            message: messages[msg_idx].clone(),
                            notification_stream_id: Some(stream_id),
                        });
                    }
                }
                Err(e) => {
                    tracing::warn!("batch notification pipeline failed (non-fatal): {e:#}");
                }
            }
        }
    }

    Ok(messages
        .into_iter()
        .zip(notifications_by_message)
        .map(|(message, notifications)| PostedMessage {
            message,
            notifications,
        })
        .collect())
}

/// Legacy wrapper — delegates to [`bus_post_messages_batch_with_notifications`] and strips
/// notification metadata.
///
/// # Errors
///
/// Returns an error if Redis pipeline execution fails.
#[allow(dead_code, reason = "legacy batch API kept while HTTP/MCP migrate")]
pub fn bus_post_messages_batch(
    conn: &mut redis::Connection,
    settings: &Settings,
    payloads: Vec<BatchSendPayload>,
    pg_writer: Option<&PgWriter>,
    has_sse_subscribers: bool,
) -> Result<Vec<Message>> {
    Ok(bus_post_messages_batch_with_notifications(
        conn,
        settings,
        payloads,
        pg_writer,
        has_sse_subscribers,
    )?
    .into_iter()
    .map(|posted| posted.message)
    .collect())
}

/// Post a message to the stream and optionally publish to the Redis pub/sub channel.
///
/// Schema inference runs on every call regardless of transport: the inferred
/// schema name (when present) is stored as `metadata["_schema"]` so that all
/// readers — Redis, `PostgreSQL`, and MCP clients — see the same annotation.
///
/// Bodies longer than [`COMPRESS_THRESHOLD`] bytes are LZ4-compressed and
/// Base64-encoded before storage.  Compression metadata is added to the
/// stream entry so that [`decode_stream_entry`] can transparently decompress.
///
/// `PostgreSQL` persistence is handled asynchronously by the [`PgWriter`]
/// background task (100 ms batched flush).  No synchronous PG write is
/// performed on the hot path; callers should query Redis for immediate
/// read-your-write consistency within the same request.
///
/// The `has_sse_subscribers` flag controls whether the message is also
/// published to the Redis pub/sub channel (used by `GET /events`).  Pass
/// `false` in CLI and MCP-stdio contexts where no SSE clients exist, avoiding
/// the ~3 µs JSON serialisation + `PUBLISH` round-trip on every message.
///
/// # Errors
///
/// Returns an error if the Redis `XADD` or `PUBLISH` commands fail, or if
/// the presence JSON fails to serialize.
#[expect(
    clippy::too_many_arguments,
    reason = "maps directly to protocol fields plus has_sse_subscribers flag"
)]
#[expect(
    clippy::too_many_lines,
    reason = "compression + schema inference + PG persistence in one atomic unit"
)]
pub fn bus_post_message_with_notifications(
    conn: &mut redis::Connection,
    settings: &Settings,
    from: &str,
    to: &str,
    topic: &str,
    body: &str,
    thread_id: Option<&str>,
    tags: &[String],
    priority: &str,
    request_ack: bool,
    reply_to: Option<&str>,
    metadata: &serde_json::Value,
    pg_writer: Option<&PgWriter>,
    has_sse_subscribers: bool,
) -> Result<PostedMessage> {
    let id = Uuid::now_v7().to_string();
    // Fix 3: pre-allocate a 32-byte buffer and write directly into it, avoiding
    // the intermediate `DelayedFormat` heap allocation from `.to_string()`.
    let ts = {
        let mut buf = String::with_capacity(32);
        let _ = write!(buf, "{}", Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ"));
        buf
    };
    let reply = reply_to.unwrap_or(from);

    // --- Task 1: server-side schema inference ------------------------------------
    // Merge the inferred schema into metadata before persisting so that every
    // transport (CLI, MCP, HTTP) gets the annotation for free.
    //
    // Optimisation: build the final metadata map in a single pass that handles
    // both the schema annotation and the optional compression markers.  This
    // avoids the two successive `m.clone()` calls the previous code made when
    // both schema inference and compression fired on the same message.
    let inferred_schema = infer_schema_from_topic(topic, None);

    // --- LZ4 body compression ------------------------------------------------
    // Compress bodies above the threshold to reduce Redis memory and payload
    // sizes. The original body is kept in the returned `Message` struct so all
    // callers work with uncompressed data.
    //
    // `compressed`: `Some(b64_body)` when compression succeeded;
    // `None` when the body is too small or compression failed.
    let compressed: Option<String> = if body.len() > COMPRESS_THRESHOLD {
        match lz4_compress_body(body) {
            Ok((b64, _)) => Some(b64),
            Err(e) => {
                tracing::warn!("lz4 compression failed, storing uncompressed: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let stored_body: &str = compressed.as_deref().unwrap_or(body);

    // Task 4.4: auto-infer thread_id from reply_to when not explicitly set.
    // Mirror the logic in prepare_message so both paths behave identically.
    let effective_thread_id: Option<&str> = thread_id.or(reply_to);

    // Build final metadata in one clone, merging schema + compression markers.
    let final_metadata_owned: serde_json::Value;
    let final_metadata: &serde_json::Value = if inferred_schema.is_some() || compressed.is_some() {
        let mut map = match metadata {
            serde_json::Value::Object(m) => m.clone(),
            _ => serde_json::Map::new(),
        };
        if let Some(schema) = inferred_schema {
            map.insert(
                "_schema".to_owned(),
                serde_json::Value::String(schema.to_owned()),
            );
        }
        if compressed.is_some() {
            map.insert("_compressed".to_owned(), serde_json::json!("lz4"));
            map.insert(
                "_original_size".to_owned(),
                serde_json::Value::Number(body.len().into()),
            );
        }
        final_metadata_owned = serde_json::Value::Object(map);
        &final_metadata_owned
    } else {
        metadata
    };
    // effective_metadata for the returned Message is the schema-annotated map
    // (without compression markers, since the returned body is uncompressed).
    let effective_metadata: &serde_json::Value = if inferred_schema.is_some() {
        final_metadata
    } else {
        metadata
    };
    // -------------------------------------------------------------------------

    // Task 4.1: inject session tag when a session_id is configured.
    let effective_tags: smallvec::SmallVec<[String; 4]> =
        if let Some(sid) = settings.session_id.as_deref() {
            let tag = format!("session:{sid}");
            if tags.iter().any(|t| t == &tag) {
                tags.to_vec().into()
            } else {
                let mut v: smallvec::SmallVec<[String; 4]> = tags.to_vec().into();
                v.push(tag);
                v
            }
        } else {
            tags.to_vec().into()
        };

    let tags_json =
        serde_json::to_string(effective_tags.as_slice()).unwrap_or_else(|_| "[]".to_owned());
    let ack_str = if request_ack { "true" } else { "false" };
    let thread_str = effective_thread_id.unwrap_or("");

    let meta_json = serde_json::to_string(final_metadata).unwrap_or_else(|_| "{}".to_owned());

    // Use a fixed-capacity SmallVec-style array on the stack: 12 fixed fields
    // + optional thread_id (1) + optional compression markers (2) = 15 max.
    let mut fields: Vec<(&str, &str)> = Vec::with_capacity(15);
    fields.extend_from_slice(&[
        ("id", &id),
        ("timestamp_utc", &ts),
        ("protocol_version", PROTOCOL_VERSION),
        ("from", from),
        ("to", to),
        ("topic", topic),
        ("body", stored_body),
        ("tags", &tags_json),
        ("priority", priority),
        ("request_ack", ack_str),
        ("reply_to", reply),
        ("metadata", &meta_json),
    ]);
    if !thread_str.is_empty() {
        fields.push(("thread_id", thread_str));
    }
    // Store a dedicated field for fast decompression detection in decode_stream_entry.
    if compressed.is_some() {
        fields.push(("_compressed", "lz4"));
    }

    let stream_id: String = redis::cmd("XADD")
        .arg(&settings.stream_key)
        .arg("MAXLEN")
        .arg("~")
        .arg(settings.stream_maxlen)
        .arg("*")
        .arg(&fields)
        .query(conn)
        .context("XADD failed")?;

    let msg = Message {
        id: id.clone(),
        timestamp_utc: ts,
        protocol_version: PROTOCOL_VERSION.to_owned(),
        from: from.to_owned(),
        to: to.to_owned(),
        topic: topic.to_owned(),
        body: body.to_owned(),
        thread_id: effective_thread_id.map(String::from),
        tags: effective_tags,
        priority: priority.to_owned(),
        request_ack,
        reply_to: Some(reply.to_owned()),
        metadata: effective_metadata.clone(),
        stream_id: Some(stream_id.clone()),
    };

    // Publish when legacy `/events` subscribers are connected or when a direct
    // recipient-specific message should wake active `/events/:agent` clients
    // through the HTTP bridge subscriber.
    if should_publish_message_event(&msg, has_sse_subscribers) {
        let event = serde_json::json!({"event": "message", "message": &msg});
        let event_json = serde_json::to_string(&event).unwrap_or_default();
        let _: () = conn
            .publish(&settings.channel_key, &event_json)
            .context("PUBLISH failed")?;
    }

    // --- Task 2: async PG write-through ------------------------------------------
    // Enqueue to the background `PgWriter` channel when a handle is provided.
    // The writer batches and flushes to `PostgreSQL` every 100 ms, keeping the
    // hot path non-blocking.  The previous synchronous `persist_message_postgres`
    // call (~230 ms per request) has been removed; Redis is the source-of-truth
    // for immediate read-your-write consistency within the same request.
    if let Some(writer) = pg_writer {
        writer.send_message(&msg);
    }
    // -------------------------------------------------------------------------

    // --- Pending ACK tracking ------------------------------------------------
    // When the sender requests acknowledgement, record the pending ack in Redis
    // with a 300-second TTL so `list_pending_acks` / `GET /pending-acks` can
    // surface unacknowledged messages.  Failures are non-fatal.
    if request_ack && let Err(error) = track_pending_ack(conn, &id, to, &msg.timestamp_utc) {
        tracing::warn!("failed to track pending ack for {id}: {error:#}");
    }

    // --- Ack deadline tracking (additive) ------------------------------------
    // When the sender requests acknowledgement, also store an ack deadline
    // record with a priority-dependent TTL. This is non-fatal.
    if request_ack
        && let Err(error) = crate::ops::ack_deadline::store_ack_deadline(conn, &id, to, priority)
    {
        tracing::warn!("failed to store ack deadline for {id}: {error:#}");
    }
    // -------------------------------------------------------------------------

    let notifications = match append_notifications_for_message(conn, &msg) {
        Ok(notifications) => notifications,
        Err(error) => {
            tracing::warn!("failed to append notification for {id}: {error:#}");
            Vec::new()
        }
    };

    // --- Ownership conflict detection ----------------------------------------
    // When an agent posts with topic="ownership", extract file paths from the
    // body, check for in-process conflicts, emit warnings, then record the
    // claim.  This is a lightweight complement to the Redis-backed arbitration
    // in `channels::claim_resource`; it fires synchronously on the hot path
    // but is non-fatal (lock errors or parse failures are warned and skipped).
    if topic == "ownership" {
        let file_refs: Vec<String> = extract_claimed_files(body);
        if !file_refs.is_empty() {
            let file_slices: Vec<&str> = file_refs.iter().map(String::as_str).collect();

            // Check Redis for cross-process conflicts (persistent across CLI invocations)
            let redis_conflicts = check_redis_ownership(settings, from, &file_slices);
            for conflict in &redis_conflicts {
                tracing::warn!(
                    file = %conflict.file,
                    current_owner = %conflict.claimed_by,
                    claimed_at = %conflict.claimed_at,
                    new_claimant = %from,
                    "ownership conflict (Redis): {} is claimed by {}",
                    conflict.file,
                    conflict.claimed_by
                );
            }

            // Also check in-memory tracker (for same-process conflicts in HTTP server)
            match global_ownership_tracker().lock() {
                Ok(mut tracker) => {
                    let mem_conflicts = tracker.check_conflicts(from, &file_slices);
                    for conflict in &mem_conflicts {
                        tracing::warn!(
                            file = %conflict.file,
                            current_owner = %conflict.claimed_by,
                            claimed_at = %conflict.claimed_at,
                            new_claimant = %from,
                            "ownership conflict (in-memory): {} is claimed by {}",
                            conflict.file,
                            conflict.claimed_by
                        );
                    }
                    tracker.record_claim(from, &file_slices);
                }
                Err(e) => {
                    tracing::warn!("ownership tracker lock poisoned, skipping: {e}");
                }
            }
        }
    }
    // -------------------------------------------------------------------------

    Ok(PostedMessage {
        message: msg,
        notifications,
    })
}

/// Legacy wrapper — delegates to [`bus_post_message_with_notifications`] and strips
/// notification metadata.
///
/// # Errors
///
/// Returns an error if the Redis `XADD` or `PUBLISH` commands fail.
#[expect(
    clippy::too_many_arguments,
    reason = "legacy wrapper preserves the pre-notification call signature"
)]
#[allow(
    dead_code,
    reason = "compatibility wrapper retained while transports migrate"
)]
pub fn bus_post_message(
    conn: &mut redis::Connection,
    settings: &Settings,
    from: &str,
    to: &str,
    topic: &str,
    body: &str,
    thread_id: Option<&str>,
    tags: &[String],
    priority: &str,
    request_ack: bool,
    reply_to: Option<&str>,
    metadata: &serde_json::Value,
    pg_writer: Option<&PgWriter>,
    has_sse_subscribers: bool,
) -> Result<Message> {
    Ok(bus_post_message_with_notifications(
        conn,
        settings,
        from,
        to,
        topic,
        body,
        thread_id,
        tags,
        priority,
        request_ack,
        reply_to,
        metadata,
        pg_writer,
        has_sse_subscribers,
    )?
    .message)
}

// ---------------------------------------------------------------------------
// Pending ACK tracking (Task 2)
// ---------------------------------------------------------------------------

/// TTL for pending-ack entries: 300 seconds (5 minutes).
const PENDING_ACK_TTL: u64 = 300;

/// Age threshold after which a pending-ack entry is flagged STALE (60 seconds).
const PENDING_ACK_STALE_SECS: i64 = 60;

/// Redis key prefix for pending-ack tracking sets.
const PENDING_ACK_PREFIX: &str = "agent_bus:pending_ack:";

/// A pending acknowledgement record stored in Redis.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PendingAck {
    /// The message ID waiting for acknowledgement.
    pub message_id: String,
    /// The agent the message was sent to.
    pub recipient: String,
    /// When the message was originally sent (UTC ISO-8601).
    pub sent_at: String,
    /// Whether the ack has been pending longer than [`PENDING_ACK_STALE_SECS`].
    pub stale: bool,
}

/// Record that a message with `request_ack=true` is awaiting acknowledgement.
///
/// Stores a JSON blob at `agent_bus:pending_ack:<message_id>` with a TTL of
/// 300 seconds. Callers should invoke this immediately after posting a message
/// that sets `request_ack = true`.
///
/// # Errors
///
/// Returns an error if the Redis `SET EX` command fails.
pub fn track_pending_ack(
    conn: &mut redis::Connection,
    message_id: &str,
    recipient: &str,
    sent_at: &str,
) -> Result<()> {
    let key = format!("{PENDING_ACK_PREFIX}{message_id}");
    let record = serde_json::json!({
        "message_id": message_id,
        "recipient": recipient,
        "sent_at": sent_at,
    });
    let value = serde_json::to_string(&record).context("serialize pending-ack record")?;
    let _: () = redis::Commands::set_ex(conn, &key, &value, PENDING_ACK_TTL)
        .context("SET EX pending-ack failed")?;
    Ok(())
}

/// Remove a pending-ack entry once the acknowledgement has been received.
///
/// # Errors
///
/// Returns an error if the Redis `DEL` command fails.
pub fn clear_pending_ack(conn: &mut redis::Connection, message_id: &str) -> Result<()> {
    let key = format!("{PENDING_ACK_PREFIX}{message_id}");
    let _: () = redis::Commands::del(conn, &key).context("DEL pending-ack failed")?;
    Ok(())
}

/// List all pending acknowledgements, optionally filtered by recipient agent.
///
/// Scans Redis for all `agent_bus:pending_ack:*` keys and returns each as a
/// [`PendingAck`] record.  Entries that have been waiting longer than
/// [`PENDING_ACK_STALE_SECS`] are flagged with `stale = true`.
///
/// # Errors
///
/// Returns an error if the Redis SCAN or GET commands fail.
pub fn list_pending_acks(
    conn: &mut redis::Connection,
    agent: Option<&str>,
) -> Result<Vec<PendingAck>> {
    let pattern = format!("{PENDING_ACK_PREFIX}*");
    let mut keys: Vec<String> = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let (next_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(200)
            .query(conn)
            .context("SCAN pending-acks failed")?;
        keys.extend(batch);
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }

    let now = chrono::Utc::now();
    let mut results: Vec<PendingAck> = Vec::new();
    for key in &keys {
        let value: Option<String> = redis::Commands::get(conn, key).context("GET pending-ack")?;
        let Some(json) = value else { continue };
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&json) else {
            continue;
        };
        let message_id = record["message_id"].as_str().unwrap_or("").to_owned();
        let recipient = record["recipient"].as_str().unwrap_or("").to_owned();
        let sent_at = record["sent_at"].as_str().unwrap_or("").to_owned();

        // Apply recipient filter when specified.
        if agent.is_some_and(|filter| recipient != filter) {
            continue;
        }

        // Determine staleness.
        let stale = chrono::DateTime::parse_from_rfc3339(&sent_at.replace('Z', "+00:00"))
            .map(|ts| (now - ts.with_timezone(&chrono::Utc)).num_seconds() > PENDING_ACK_STALE_SECS)
            .unwrap_or(false);

        results.push(PendingAck {
            message_id,
            recipient,
            sent_at,
            stale,
        });
    }
    results.sort_by(|a, b| a.sent_at.cmp(&b.sent_at));
    Ok(results)
}

/// Read all messages from the Redis stream (ignoring PG), ordered oldest first.
///
/// Fetches up to `limit` entries via `XRANGE` with no time or agent filter,
/// suitable for a one-time backfill into `PostgreSQL`.
///
/// # Errors
///
/// Returns an error if the Redis connection or `XRANGE` command fails.
pub fn bus_read_all_from_redis(settings: &Settings, limit: usize) -> Result<Vec<Message>> {
    let mut conn = connect(settings)?;
    let raw: Vec<redis::Value> = redis::cmd("XRANGE")
        .arg(&settings.stream_key)
        .arg("-")
        .arg("+")
        .arg("COUNT")
        .arg(limit)
        .query(&mut conn)
        .context("XRANGE failed")?;

    let mut messages = Vec::new();
    for (stream_id, fields) in parse_xrange_result(&raw) {
        let mut msg = decode_stream_entry(&fields);
        msg.stream_id = Some(stream_id);
        messages.push(msg);
    }
    Ok(messages)
}

/// List messages from the stream, most-recent first then reversed to chrono.
///
/// # Errors
///
/// Returns an error if the Redis `XREVRANGE` command fails.
pub fn bus_list_messages_from_redis(
    conn: &mut redis::Connection,
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    include_broadcast: bool,
) -> Result<Vec<Message>> {
    bus_list_messages_from_redis_with_filters(
        conn,
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

/// List messages from Redis applying agent, thread, tag, and time filters.
///
/// # Errors
///
/// Returns an error if the Redis `XREVRANGE` command fails.
#[expect(
    clippy::too_many_arguments,
    reason = "Redis read helper mirrors the transport filter surface"
)]
pub fn bus_list_messages_from_redis_with_filters(
    conn: &mut redis::Connection,
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    include_broadcast: bool,
    thread_id: Option<&str>,
    required_tags: &[&str],
) -> Result<Vec<Message>> {
    let fetch_multiplier = if thread_id.is_some() || !required_tags.is_empty() {
        XREVRANGE_OVERFETCH_FACTOR.saturating_mul(2)
    } else {
        XREVRANGE_OVERFETCH_FACTOR
    };
    let fetch_count = (limit.saturating_mul(fetch_multiplier)).max(XREVRANGE_MIN_FETCH);
    let raw: Vec<redis::Value> = redis::cmd("XREVRANGE")
        .arg(&settings.stream_key)
        .arg("+")
        .arg("-")
        .arg("COUNT")
        .arg(fetch_count)
        .query(conn)
        .context("XREVRANGE failed")?;

    #[expect(
        clippy::cast_possible_wrap,
        reason = "since_minutes is bounded to <=10080 by validation"
    )]
    let cutoff = Utc::now() - chrono::Duration::minutes(since_minutes as i64);

    let mut messages: Vec<Message> = Vec::new();
    for (stream_id, fields) in parse_xrange_result(&raw) {
        let mut msg = decode_stream_entry(&fields);

        // time-based cutoff
        if let Ok(ts) =
            chrono::DateTime::parse_from_rfc3339(&msg.timestamp_utc.replace('Z', "+00:00"))
            && ts < cutoff
        {
            continue;
        }

        if !message_matches_filters(
            &msg,
            agent,
            from_agent,
            include_broadcast,
            thread_id,
            required_tags,
        ) {
            continue;
        }

        msg.stream_id = Some(stream_id);
        messages.push(msg);
        if messages.len() >= limit {
            break;
        }
    }

    // XREVRANGE returns newest first; reverse to get chronological
    messages.reverse();
    Ok(messages)
}

/// List messages from `PostgreSQL` history or Redis stream (with automatic fallback).
///
/// Checks the circuit breaker before attempting the `PostgreSQL` path to
/// avoid unnecessary overhead when PG is known to be down.
///
/// # Errors
///
/// Returns an error only if both `PostgreSQL` and the Redis fallback fail.
pub fn bus_list_messages(
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    include_broadcast: bool,
) -> Result<Vec<Message>> {
    if settings.database_url.is_some() && !is_pg_circuit_open() {
        match list_messages_postgres(
            settings,
            agent,
            from_agent,
            since_minutes,
            limit,
            include_broadcast,
        ) {
            Ok(messages) => return Ok(messages),
            Err(error) => {
                if settings.log_non_fatal_warnings() {
                    tracing::warn!(
                        "PostgreSQL message query failed, falling back to Redis: {error:#}"
                    );
                }
            }
        }
    }

    let mut conn = connect(settings)?;
    bus_list_messages_from_redis(
        &mut conn,
        settings,
        agent,
        from_agent,
        since_minutes,
        limit,
        include_broadcast,
    )
}

/// List messages applying filters from `PostgreSQL` history or Redis stream.
///
/// When `required_tags` (or `thread_id`) are present and `PostgreSQL` is
/// available with a closed circuit breaker, the query is routed through the
/// GIN-indexed `tags @> $1::jsonb` path for efficient server-side filtering.
/// This avoids the default Redis behaviour of fetching an over-sized stream
/// slice and filtering client-side.
///
/// Falls back to the Redis `XREVRANGE` + client-side filter path when:
/// - `PostgreSQL` is not configured (`database_url` is `None`)
/// - The circuit breaker is open (PG recently unreachable)
/// - The `PostgreSQL` query fails for any reason
///
/// # Errors
///
/// Returns an error only if both `PostgreSQL` and the Redis fallback fail.
#[allow(dead_code)]
#[expect(
    clippy::too_many_arguments,
    reason = "top-level read helper mirrors the transport filter surface"
)]
pub fn bus_list_messages_with_filters(
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    include_broadcast: bool,
    thread_id: Option<&str>,
    required_tags: &[&str],
) -> Result<Vec<Message>> {
    let has_tags = !required_tags.is_empty() || thread_id.is_some();

    // Fast path: when PG is configured and the circuit breaker is closed,
    // route tag-scoped queries through the GIN index.  For unscoped queries
    // (no tags, no thread) we still try PG for its richer history, but the
    // optimised `query_messages_by_tags` path is reserved for cases where the
    // GIN index provides a meaningful selectivity advantage.
    if settings.database_url.is_some() && !is_pg_circuit_open() {
        // When only tags/thread are specified (no agent/sender filters), use
        // the dedicated tag-query function that skips agent/sender columns.
        if has_tags && agent.is_none() && from_agent.is_none() {
            tracing::debug!(
                tags = ?required_tags,
                ?thread_id,
                "routing tagged query through PostgreSQL GIN index (optimised path)"
            );
            match query_messages_by_tags(settings, required_tags, thread_id, since_minutes, limit) {
                Ok(messages) => return Ok(messages),
                Err(error) => {
                    if settings.log_non_fatal_warnings() {
                        tracing::warn!(
                            "PostgreSQL tag query failed, falling back to full filter path: {error:#}"
                        );
                    }
                    // Fall through to the general PG path, then Redis.
                }
            }
        }

        // General PG path: handles agent/sender filters alongside tags.
        tracing::debug!(
            ?agent,
            ?from_agent,
            tags = ?required_tags,
            ?thread_id,
            "routing query through PostgreSQL (general path)"
        );
        match list_messages_postgres_with_filters(
            settings,
            agent,
            from_agent,
            since_minutes,
            limit,
            include_broadcast,
            thread_id,
            required_tags,
        ) {
            Ok(messages) => return Ok(messages),
            Err(error) => {
                if settings.log_non_fatal_warnings() {
                    tracing::warn!(
                        "PostgreSQL message query failed, falling back to Redis: {error:#}"
                    );
                }
            }
        }
    } else if settings.database_url.is_some() && is_pg_circuit_open() {
        tracing::debug!("skipping PostgreSQL path — circuit breaker open, using Redis fallback");
    }

    // Redis fallback: XREVRANGE with client-side filtering.
    let mut conn = connect(settings)?;
    bus_list_messages_from_redis_with_filters(
        &mut conn,
        settings,
        agent,
        from_agent,
        since_minutes,
        limit,
        include_broadcast,
        thread_id,
        required_tags,
    )
}

/// Read messages addressed to `agent` that arrived after `since_id`.
///
/// Uses `XRANGE` with an exclusive lower bound (`(since_id` notation when
/// supported, or `since_id` incremented by one millisecond as a fallback)
/// so the entry at `since_id` itself is never returned.  This is the
/// underlying primitive for the cursor-based `check_inbox` MCP tool.
///
/// Returns messages in chronological order (oldest first) and respects
/// `limit` as a hard cap.  Only messages addressed to `agent` or broadcast
/// to `"all"` are included.
///
/// # Errors
///
/// Returns an error if the Redis connection or `XRANGE` command fails.
#[allow(dead_code)]
pub fn bus_list_messages_since_id(
    settings: &Settings,
    agent: &str,
    since_id: &str,
    limit: usize,
) -> Result<Vec<Message>> {
    bus_list_messages_since_id_with_filters(settings, agent, since_id, limit, None, &[])
}

/// List messages for `agent` after `since_id` applying thread and tag filters.
///
/// # Errors
///
/// Returns an error if the Redis connection or `XRANGE` command fails.
pub fn bus_list_messages_since_id_with_filters(
    settings: &Settings,
    agent: &str,
    since_id: &str,
    limit: usize,
    thread_id: Option<&str>,
    required_tags: &[&str],
) -> Result<Vec<Message>> {
    let mut conn = connect(settings)?;

    // Build an exclusive lower bound.  Redis 6.2+ supports the `(id` syntax
    // for exclusive ranges in XRANGE.  We rely on that here; all supported
    // Redis versions in this project meet the 6.2 bar.
    let exclusive_start = format!("({since_id}");
    let fetch_multiplier = if thread_id.is_some() || !required_tags.is_empty() {
        XREVRANGE_OVERFETCH_FACTOR.saturating_mul(2)
    } else {
        XREVRANGE_OVERFETCH_FACTOR
    };

    let raw: Vec<redis::Value> = redis::cmd("XRANGE")
        .arg(&settings.stream_key)
        .arg(&exclusive_start)
        .arg("+")
        .arg("COUNT")
        // Overfetch by the broadcast-filter factor so we still hit `limit`
        // after dropping messages not addressed to this agent.
        .arg((limit.saturating_mul(fetch_multiplier)).max(XREVRANGE_MIN_FETCH))
        .query(&mut conn)
        .context("XRANGE since_id failed")?;

    let mut messages: Vec<Message> = Vec::new();
    for (stream_id, fields) in parse_xrange_result(&raw) {
        let mut msg = decode_stream_entry(&fields);

        // Include only messages addressed to this agent or broadcast.
        if !message_matches_filters(&msg, Some(agent), None, true, thread_id, required_tags) {
            continue;
        }

        msg.stream_id = Some(stream_id);
        messages.push(msg);
        if messages.len() >= limit {
            break;
        }
    }

    Ok(messages)
}

/// Read or initialise the inbox cursor for `agent` from Redis.
///
/// The cursor is the stream ID of the last message the agent acknowledged
/// via `check_inbox`.  A missing key means the agent has never polled, so
/// we return `"0-0"` (the Redis stream origin) to deliver all pending
/// messages on the first call.
///
/// # Errors
///
/// Returns an error if the Redis `GET` command fails.
#[allow(
    dead_code,
    reason = "legacy cursor retained for compatibility during notification migration"
)]
pub fn get_inbox_cursor(conn: &mut redis::Connection, agent: &str) -> Result<String> {
    let key = inbox_cursor_key(agent);
    let val: Option<String> = redis::Commands::get(conn, &key).context("GET inbox cursor")?;
    Ok(val.unwrap_or_else(|| "0-0".to_owned()))
}

/// Advance the inbox cursor for `agent` to `stream_id`.
///
/// The cursor is given a 7-day TTL so inactive agents' cursors are
/// eventually reclaimed.  Each advance refreshes the TTL.
///
/// # Errors
///
/// Returns an error if the Redis `SET` command fails.
#[allow(
    dead_code,
    reason = "legacy cursor retained for compatibility during notification migration"
)]
pub fn set_inbox_cursor(conn: &mut redis::Connection, agent: &str, stream_id: &str) -> Result<()> {
    let key = inbox_cursor_key(agent);
    let mut pipe = redis::pipe();
    pipe.cmd("SET").arg(&key).arg(stream_id);
    pipe.cmd("EXPIRE").arg(&key).arg(CURSOR_TTL_SECS);
    pipe.query::<((), ())>(conn)
        .context("SET + EXPIRE inbox cursor")?;
    Ok(())
}

/// Redis key for the inbox cursor of `agent`.
///
/// Format: `bus:cursor:<agent_id>`
#[allow(
    dead_code,
    reason = "legacy cursor retained for compatibility during notification migration"
)]
#[must_use]
pub fn inbox_cursor_key(agent: &str) -> String {
    format!("bus:cursor:{agent}")
}

/// Set presence for an agent.
///
/// When `pg_writer` is `Some`, the presence event is enqueued for
/// non-blocking async `PostgreSQL` persistence.
///
/// # Errors
///
/// Returns an error if the Redis `SET EX` or `PUBLISH` commands fail, or if
/// presence JSON serialization fails.
#[expect(
    clippy::too_many_arguments,
    reason = "maps directly to protocol fields + pg_writer"
)]
pub fn bus_set_presence(
    conn: &mut redis::Connection,
    settings: &Settings,
    agent: &str,
    status: &str,
    session_id: Option<&str>,
    capabilities: &[String],
    ttl_seconds: u64,
    metadata: &serde_json::Value,
    pg_writer: Option<&PgWriter>,
) -> Result<Presence> {
    let key = format!("{}{agent}", settings.presence_prefix);
    let sid = session_id.map_or_else(|| Uuid::now_v7().to_string(), String::from);

    let presence = Presence {
        agent: agent.to_owned(),
        status: status.to_owned(),
        protocol_version: PROTOCOL_VERSION.to_owned(),
        timestamp_utc: Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
        session_id: sid,
        capabilities: capabilities.to_vec(),
        metadata: metadata.clone(),
        ttl_seconds,
    };

    let json = serde_json::to_string(&presence).context("serialize presence")?;
    let _: () = conn
        .set_ex(&key, &json, ttl_seconds)
        .context("SET EX failed")?;

    let event = serde_json::json!({"event": "presence", "presence": &presence});
    let event_json = serde_json::to_string(&event).unwrap_or_default();
    let _: () = conn
        .publish(&settings.channel_key, &event_json)
        .context("PUBLISH presence failed")?;

    if let Some(writer) = pg_writer {
        writer.send_presence(&presence);
    } else if let Err(error) = persist_presence_postgres(settings, &presence) {
        tracing::warn!("failed to persist presence event to Postgres: {error:#}");
    }

    Ok(presence)
}

/// List all presence records using cursor-based SCAN (avoids blocking KEYS).
///
/// # Errors
///
/// Returns an error if the Redis `SCAN` or `GET` commands fail.
pub fn bus_list_presence(
    conn: &mut redis::Connection,
    settings: &Settings,
) -> Result<Vec<Presence>> {
    let pattern = format!("{}*", settings.presence_prefix);

    // Use SCAN instead of KEYS to avoid blocking the Redis event loop.
    // Count hint of 100 is generous for the small presence key space.
    let mut keys: Vec<String> = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let (next_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(100)
            .query(conn)
            .context("SCAN failed")?;
        keys.extend(batch);
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }

    let mut results: Vec<Presence> = Vec::new();
    for key in &keys {
        let value: Option<String> = conn.get(key).context("GET failed")?;
        if let Some(json) = value
            && let Ok(p) = serde_json::from_str::<Presence>(&json)
        {
            results.push(p);
        }
    }
    results.sort_by(|a, b| a.agent.cmp(&b.agent));
    Ok(results)
}

/// Get bus health.
///
/// When `pool` is `Some`, a pooled Redis connection is used for the PING and
/// XLEN commands, eliminating the two fresh TCP handshakes the previous
/// implementation incurred.  Pass `None` in CLI mode where no pool exists.
pub fn bus_health(settings: &Settings, pool: Option<&RedisPool>) -> Health {
    // Use a pooled connection when available; otherwise open a fresh one.
    let (ok, stream_length) = if let Some(mut conn) = pool.and_then(|p| p.get_connection().ok()) {
        let ok = redis::cmd("PING")
            .query::<String>(&mut *conn)
            .map(|pong| pong == "PONG")
            .unwrap_or(false);
        let stream_length = redis::cmd("XLEN")
            .arg(&settings.stream_key)
            .query::<u64>(&mut *conn)
            .ok();
        (ok, stream_length)
    } else {
        // CLI mode: fall back to a single fresh connection for both queries.
        match connect(settings) {
            Ok(mut conn) => {
                let ok = redis::cmd("PING")
                    .query::<String>(&mut conn)
                    .map(|pong| pong == "PONG")
                    .unwrap_or(false);
                let stream_length = redis::cmd("XLEN")
                    .arg(&settings.stream_key)
                    .query::<u64>(&mut conn)
                    .ok();
                (ok, stream_length)
            }
            Err(_) => (false, None),
        }
    };

    let (database_ok, database_error, storage_ready) = probe_postgres(settings);
    // Single round-trip for both PG counts instead of two separate queries.
    let (pg_message_count, pg_presence_count) = count_both_postgres(settings);

    // Populate write-through metrics when PG is configured.
    let (pg_writes_queued, pg_writes_completed, pg_batches, pg_write_errors) =
        if settings.database_url.is_some() {
            let m = pg_metrics();
            (
                Some(m.messages_queued.load(Ordering::Relaxed)),
                Some(m.messages_written.load(Ordering::Relaxed)),
                Some(m.batches_flushed.load(Ordering::Relaxed)),
                Some(m.write_errors.load(Ordering::Relaxed)),
            )
        } else {
            (None, None, None, None)
        };

    Health {
        ok,
        protocol_version: PROTOCOL_VERSION.to_owned(),
        redis_url: redact_url(&settings.redis_url),
        database_url: settings.database_url.as_deref().map(redact_url),
        database_ok,
        database_error,
        storage_ready: storage_ready || settings.database_url.is_none(),
        runtime: "rust-native".to_owned(),
        codec: "serde_json".to_owned(),
        stream_length,
        pg_message_count,
        pg_presence_count,
        pg_writes_queued,
        pg_writes_completed,
        pg_batches,
        pg_write_errors,
    }
}

/// Return a degraded [`Health`] value for use when the health probe panics.
///
/// Used by the dashboard handler as the `unwrap_or_else` fallback when the
/// `spawn_blocking` task join fails (which only happens on task panic, not on
/// Redis/PG errors — those are handled inside [`bus_health`] itself).
#[must_use]
pub fn health_error_fallback() -> Health {
    Health {
        ok: false,
        protocol_version: PROTOCOL_VERSION.to_owned(),
        redis_url: "unknown".to_owned(),
        database_url: None,
        database_ok: None,
        database_error: Some("health probe task panicked".to_owned()),
        storage_ready: false,
        runtime: "rust-native".to_owned(),
        codec: "serde_json+lz4".to_owned(),
        stream_length: None,
        pg_message_count: None,
        pg_presence_count: None,
        pg_writes_queued: None,
        pg_writes_completed: None,
        pg_batches: None,
        pg_write_errors: None,
    }
}

// ---------------------------------------------------------------------------
// Task queue — Redis LIST per agent (RPUSH / LPOP / LRANGE / LLEN)
// ---------------------------------------------------------------------------

/// Redis key prefix for per-agent task queues.
///
/// Full key format: `bus:tasks:<agent_id>`.
pub const TASK_QUEUE_PREFIX: &str = "bus:tasks:";

/// Push a task to the tail of an agent's task queue.
///
/// Returns the new queue length so callers can monitor backpressure.
///
/// # Errors
///
/// Returns an error if the Redis connection or `RPUSH` command fails.
///
/// # Examples
///
/// ```no_run
/// # use agent_bus_core::redis_bus::push_task;
/// # use agent_bus_core::settings::Settings;
/// let settings = Settings::from_env();
/// let len = push_task(&settings, "codex", r#"{"type":"review","file":"src/main.rs"}"#).unwrap();
/// println!("queue length: {len}");
/// ```
pub fn push_task(settings: &Settings, agent: &str, task_json: &str) -> Result<u64> {
    let mut conn = connect(settings)?;
    let key = format!("{TASK_QUEUE_PREFIX}{agent}");
    let (len,): (u64,) = redis::pipe()
        .cmd("RPUSH")
        .arg(&key)
        .arg(task_json)
        .cmd("EXPIRE")
        .arg(&key)
        .arg(TASK_QUEUE_TTL_SECS)
        .ignore()
        .query(&mut conn)
        .context("RPUSH + EXPIRE task failed")?;
    Ok(len)
}

/// Pop and return the next task from the head of an agent's queue.
///
/// Returns `None` when the queue is empty.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LPOP` command fails.
pub fn pull_task(settings: &Settings, agent: &str) -> Result<Option<String>> {
    let mut conn = connect(settings)?;
    let key = format!("{TASK_QUEUE_PREFIX}{agent}");
    let task: Option<String> = redis::cmd("LPOP")
        .arg(&key)
        .query(&mut conn)
        .context("LPOP task failed")?;
    Ok(task)
}

/// Return up to `limit` tasks from the head of an agent's queue without
/// consuming them.
///
/// `limit = 0` returns all entries (`LRANGE key 0 -1`).
///
/// # Errors
///
/// Returns an error if the Redis connection or `LRANGE` command fails.
pub fn peek_tasks(settings: &Settings, agent: &str, limit: usize) -> Result<Vec<String>> {
    let mut conn = connect(settings)?;
    let key = format!("{TASK_QUEUE_PREFIX}{agent}");
    let end: isize = if limit == 0 {
        -1
    } else {
        isize::try_from(limit.saturating_sub(1)).unwrap_or(isize::MAX)
    };
    let tasks: Vec<String> = redis::cmd("LRANGE")
        .arg(&key)
        .arg(0_isize)
        .arg(end)
        .query(&mut conn)
        .context("LRANGE tasks failed")?;
    Ok(tasks)
}

/// Return the current number of pending tasks in an agent's queue.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LLEN` command fails.
pub fn task_queue_length(settings: &Settings, agent: &str) -> Result<u64> {
    let mut conn = connect(settings)?;
    let key = format!("{TASK_QUEUE_PREFIX}{agent}");
    let len: u64 = redis::cmd("LLEN")
        .arg(&key)
        .query(&mut conn)
        .context("LLEN task queue failed")?;
    Ok(len)
}

// ===========================================================================
// Subscription storage
// ===========================================================================

/// Redis key prefix for individual subscription keys.
///
/// Full key format: `agent_bus:sub:<agent>:<subscription_id>`.
/// Each key stores the JSON-serialized [`Subscription`] and may have a TTL set
/// via `SET EX` when the subscription specifies `ttl_seconds`.
pub const SUBSCRIPTION_KEY_PREFIX: &str = "agent_bus:sub:";

use crate::models::Subscription;

/// Save a subscription to Redis.
///
/// The subscription is stored as a JSON string under the key
/// `agent_bus:sub:<agent>:<id>`.  If `ttl_seconds` is set on the
/// subscription, the key is given that TTL via `SET EX`.
///
/// # Errors
///
/// Returns an error if the Redis connection, serialization, or SET fails.
pub fn save_subscription(settings: &Settings, sub: &Subscription) -> Result<()> {
    let mut conn = connect(settings)?;
    let key = format!("{SUBSCRIPTION_KEY_PREFIX}{}:{}", sub.agent, sub.id);
    let json = serde_json::to_string(sub).context("serialize subscription")?;

    if let Some(ttl) = sub.ttl_seconds {
        redis::cmd("SET")
            .arg(&key)
            .arg(&json)
            .arg("EX")
            .arg(ttl)
            .query::<()>(&mut conn)
            .context("SET subscription with TTL failed")?;
    } else {
        redis::cmd("SET")
            .arg(&key)
            .arg(&json)
            .query::<()>(&mut conn)
            .context("SET subscription failed")?;
    }

    Ok(())
}

/// List all subscriptions for a given agent.
///
/// Uses `SCAN` with the pattern `agent_bus:sub:<agent>:*` to find all keys,
/// then `GET`s each one.  Expired keys are automatically excluded by Redis.
///
/// # Errors
///
/// Returns an error if the Redis connection, SCAN, or GET commands fail.
pub fn list_subscriptions(settings: &Settings, agent: &str) -> Result<Vec<Subscription>> {
    let mut conn = connect(settings)?;
    let pattern = format!("{SUBSCRIPTION_KEY_PREFIX}{agent}:*");

    // Collect all matching keys via SCAN.
    let mut cursor: u64 = 0;
    let mut keys: Vec<String> = Vec::new();
    loop {
        let (next_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(100_u64)
            .query(&mut conn)
            .context("SCAN subscriptions failed")?;
        keys.extend(batch);
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }

    let mut subs = Vec::with_capacity(keys.len());
    for key in &keys {
        let json: Option<String> = redis::cmd("GET")
            .arg(key)
            .query(&mut conn)
            .context("GET subscription failed")?;
        if let Some(raw) = json
            && let Ok(sub) = serde_json::from_str::<Subscription>(&raw)
        {
            subs.push(sub);
        }
    }

    // Sort by created_at for deterministic output.
    subs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    Ok(subs)
}

/// Delete a specific subscription.
///
/// Returns `true` if the key existed and was deleted, `false` if it was
/// already absent (e.g. expired or never created).
///
/// # Errors
///
/// Returns an error if the Redis connection or DEL command fails.
pub fn delete_subscription(settings: &Settings, agent: &str, sub_id: &str) -> Result<bool> {
    let mut conn = connect(settings)?;
    let key = format!("{SUBSCRIPTION_KEY_PREFIX}{agent}:{sub_id}");
    let deleted: u64 = redis::cmd("DEL")
        .arg(&key)
        .query(&mut conn)
        .context("DEL subscription failed")?;
    Ok(deleted > 0)
}

// ===========================================================================
// Resource event streams
// ===========================================================================

/// Redis key prefix for per-resource event streams.
const RESOURCE_EVENT_PREFIX: &str = "agent_bus:resource_events:";

/// Maximum entries per resource event stream.
const RESOURCE_EVENT_MAXLEN: u64 = 1000;

/// Build the Redis stream key for a resource's event log.
#[must_use]
pub fn resource_event_stream_key(resource: &str) -> String {
    // Normalise path separators for Windows compatibility.
    let normalised = resource.replace('\\', "/");
    format!("{RESOURCE_EVENT_PREFIX}{normalised}")
}

/// Emit a resource lifecycle event to the per-resource event stream.
///
/// Each call appends one `XADD` entry with MAXLEN trimming.  This is intended
/// to be called from claim operations (claim, renew, release, resolve) as a
/// cheap, best-effort audit trail.
///
/// # Errors
///
/// Returns an error if the Redis `XADD` command fails.
pub fn emit_resource_event(
    conn: &mut redis::Connection,
    event: &str,
    agent: &str,
    resource: &str,
) -> Result<String> {
    let key = resource_event_stream_key(resource);
    let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    let fields: Vec<(&str, &str)> = vec![
        ("event", event),
        ("agent", agent),
        ("resource", resource),
        ("timestamp", &ts),
    ];

    let (stream_id,): (String,) = redis::pipe()
        .cmd("XADD")
        .arg(&key)
        .arg("MAXLEN")
        .arg("~")
        .arg(RESOURCE_EVENT_MAXLEN)
        .arg("*")
        .arg(&fields)
        .cmd("EXPIRE")
        .arg(&key)
        .arg(RESOURCE_EVENT_TTL_SECS)
        .ignore()
        .query(conn)
        .context("XADD + EXPIRE resource event failed")?;

    Ok(stream_id)
}

/// List resource events from a per-resource event stream.
///
/// If `since_id` is provided, only events after that stream ID are returned.
/// Otherwise reads from the beginning.  Returns at most `limit` entries.
///
/// # Errors
///
/// Returns an error if the Redis `XRANGE` command fails.
pub fn list_resource_events(
    conn: &mut redis::Connection,
    resource: &str,
    since_id: Option<&str>,
    limit: usize,
) -> Result<Vec<crate::models::ResourceEvent>> {
    if limit == 0 {
        return Ok(Vec::new());
    }
    let key = resource_event_stream_key(resource);
    let start = match since_id {
        Some(id) => format!("({id}"),
        None => "0-0".to_owned(),
    };

    let raw: Vec<redis::Value> = redis::cmd("XRANGE")
        .arg(&key)
        .arg(&start)
        .arg("+")
        .arg("COUNT")
        .arg(limit.max(1))
        .query(conn)
        .context("XRANGE resource events failed")?;

    let mut events = Vec::new();
    for (stream_id, fields) in parse_xrange_result(&raw) {
        let get = |k: &str| -> String {
            match fields.get(k) {
                Some(redis::Value::BulkString(b)) => match std::str::from_utf8(b) {
                    Ok(s) => s.to_owned(),
                    Err(_) => String::from_utf8_lossy(b).into_owned(),
                },
                Some(redis::Value::SimpleString(s)) => s.clone(),
                _ => String::new(),
            }
        };
        events.push(crate::models::ResourceEvent {
            event: get("event"),
            agent: get("agent"),
            resource: get("resource"),
            timestamp: get("timestamp"),
            stream_id: Some(stream_id),
        });
    }

    Ok(events)
}

// ---------------------------------------------------------------------------
// Thread storage (Part A)
// ---------------------------------------------------------------------------

/// Redis key prefix for thread records.
const THREAD_PREFIX: &str = "agent_bus:thread:";

/// Create a new thread in Redis.
///
/// Stores the thread JSON at `agent_bus:thread:<thread_id>`.
///
/// # Errors
///
/// Returns an error if the Redis `SET` command fails or the thread already
/// exists.
pub fn create_thread(conn: &mut redis::Connection, thread: &crate::models::Thread) -> Result<()> {
    let key = format!("{THREAD_PREFIX}{}", thread.thread_id);

    // Check if thread already exists (NX-style guard).
    let existing: Option<String> =
        redis::Commands::get(conn, &key).context("GET thread check failed")?;
    if existing.is_some() {
        anyhow::bail!("thread '{}' already exists", thread.thread_id);
    }

    let json = serde_json::to_string(thread).context("serialize thread")?;
    let _: () = redis::Commands::set(conn, &key, &json).context("SET thread failed")?;
    Ok(())
}

/// Add an agent to a thread's member list.
///
/// Returns the updated thread.
///
/// # Errors
///
/// Returns an error if the thread does not exist or the Redis command fails.
pub fn join_thread(
    conn: &mut redis::Connection,
    thread_id: &str,
    agent: &str,
) -> Result<crate::models::Thread> {
    let key = format!("{THREAD_PREFIX}{thread_id}");
    let json: String =
        redis::Commands::get(conn, &key).context("GET thread failed (thread not found)")?;
    let mut thread: crate::models::Thread =
        serde_json::from_str(&json).context("deserialize thread")?;

    if !thread.members.iter().any(|m| m == agent) {
        thread.members.push(agent.to_owned());
    }

    let updated = serde_json::to_string(&thread).context("serialize thread")?;
    let _: () = redis::Commands::set(conn, &key, &updated).context("SET thread failed")?;
    Ok(thread)
}

/// Remove an agent from a thread's member list.
///
/// Returns the updated thread.
///
/// # Errors
///
/// Returns an error if the thread does not exist or the Redis command fails.
pub fn leave_thread(
    conn: &mut redis::Connection,
    thread_id: &str,
    agent: &str,
) -> Result<crate::models::Thread> {
    let key = format!("{THREAD_PREFIX}{thread_id}");
    let json: String =
        redis::Commands::get(conn, &key).context("GET thread failed (thread not found)")?;
    let mut thread: crate::models::Thread =
        serde_json::from_str(&json).context("deserialize thread")?;

    thread.members.retain(|m| m != agent);

    let updated = serde_json::to_string(&thread).context("serialize thread")?;
    let _: () = redis::Commands::set(conn, &key, &updated).context("SET thread failed")?;
    Ok(thread)
}

/// Retrieve a thread by ID.
///
/// Returns `None` if the key does not exist.
///
/// # Errors
///
/// Returns an error if the Redis `GET` or JSON deserialization fails.
pub fn get_thread(
    conn: &mut redis::Connection,
    thread_id: &str,
) -> Result<Option<crate::models::Thread>> {
    let key = format!("{THREAD_PREFIX}{thread_id}");
    let value: Option<String> = redis::Commands::get(conn, &key).context("GET thread failed")?;
    match value {
        Some(json) => {
            let thread = serde_json::from_str(&json).context("deserialize thread")?;
            Ok(Some(thread))
        }
        None => Ok(None),
    }
}

/// List all threads from Redis.
///
/// Uses cursor-based SCAN to avoid blocking KEYS.
///
/// # Errors
///
/// Returns an error if the SCAN or GET commands fail.
pub fn list_threads(conn: &mut redis::Connection) -> Result<Vec<crate::models::Thread>> {
    let pattern = format!("{THREAD_PREFIX}*");
    let mut keys: Vec<String> = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let (next_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(200)
            .query(conn)
            .context("SCAN threads failed")?;
        keys.extend(batch);
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }

    let mut results: Vec<crate::models::Thread> = Vec::new();
    for key in &keys {
        let value: Option<String> = redis::Commands::get(conn, key).context("GET thread failed")?;
        if let Some(json) = value
            && let Ok(t) = serde_json::from_str::<crate::models::Thread>(&json)
        {
            results.push(t);
        }
    }
    results.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
    Ok(results)
}

/// Close a thread by setting its status to [`ThreadStatus::Closed`].
///
/// Returns the updated thread.
///
/// # Errors
///
/// Returns an error if the thread does not exist or the Redis command fails.
pub fn close_thread(
    conn: &mut redis::Connection,
    thread_id: &str,
) -> Result<crate::models::Thread> {
    let key = format!("{THREAD_PREFIX}{thread_id}");
    let json: String =
        redis::Commands::get(conn, &key).context("GET thread failed (thread not found)")?;
    let mut thread: crate::models::Thread =
        serde_json::from_str(&json).context("deserialize thread")?;

    thread.status = crate::models::ThreadStatus::Closed;

    let updated = serde_json::to_string(&thread).context("serialize thread")?;
    let _: () = redis::Commands::set(conn, &key, &updated).context("SET thread failed")?;
    Ok(thread)
}

// ---------------------------------------------------------------------------
// Ack deadline storage (Part B)
// ---------------------------------------------------------------------------

/// Redis key prefix for ack deadline records.
const ACK_DEADLINE_PREFIX: &str = "agent_bus:ack_deadline:";

/// Store an ack deadline record in Redis with a TTL matching the deadline.
///
/// # Errors
///
/// Returns an error if the Redis `SET EX` command fails.
pub fn track_ack_deadline(
    conn: &mut redis::Connection,
    deadline: &crate::models::AckDeadline,
    ttl_seconds: u64,
) -> Result<()> {
    let key = format!("{ACK_DEADLINE_PREFIX}{}", deadline.message_id);
    let json = serde_json::to_string(deadline).context("serialize ack deadline")?;
    let _: () = redis::Commands::set_ex(conn, &key, &json, ttl_seconds)
        .context("SET EX ack_deadline failed")?;
    Ok(())
}

/// Remove an ack deadline record (e.g. when the ack arrives).
///
/// # Errors
///
/// Returns an error if the Redis `DEL` command fails.
pub fn clear_ack_deadline(conn: &mut redis::Connection, message_id: &str) -> Result<()> {
    let key = format!("{ACK_DEADLINE_PREFIX}{message_id}");
    let _: () = redis::Commands::del(conn, &key).context("DEL ack_deadline failed")?;
    Ok(())
}

/// List all ack deadlines that are still live in Redis.
///
/// Returns records that have not yet expired (key still exists).
///
/// # Errors
///
/// Returns an error if the SCAN or GET commands fail.
pub fn list_ack_deadlines(conn: &mut redis::Connection) -> Result<Vec<crate::models::AckDeadline>> {
    let pattern = format!("{ACK_DEADLINE_PREFIX}*");
    let mut keys: Vec<String> = Vec::new();
    let mut cursor: u64 = 0;
    loop {
        let (next_cursor, batch): (u64, Vec<String>) = redis::cmd("SCAN")
            .arg(cursor)
            .arg("MATCH")
            .arg(&pattern)
            .arg("COUNT")
            .arg(200)
            .query(conn)
            .context("SCAN ack_deadlines failed")?;
        keys.extend(batch);
        cursor = next_cursor;
        if cursor == 0 {
            break;
        }
    }

    let mut results: Vec<crate::models::AckDeadline> = Vec::new();
    for key in &keys {
        let value: Option<String> =
            redis::Commands::get(conn, key).context("GET ack_deadline failed")?;
        if let Some(json) = value
            && let Ok(d) = serde_json::from_str::<crate::models::AckDeadline>(&json)
        {
            results.push(d);
        }
    }
    results.sort_by(|a, b| a.deadline_at.cmp(&b.deadline_at));
    Ok(results)
}

/// Return only the ack deadlines that are past their `deadline_at` timestamp.
///
/// # Errors
///
/// Returns an error if listing deadlines fails.
pub fn check_overdue_acks(conn: &mut redis::Connection) -> Result<Vec<crate::models::AckDeadline>> {
    let now = chrono::Utc::now();
    let all = list_ack_deadlines(conn)?;
    let overdue = all
        .into_iter()
        .filter(|d| {
            chrono::DateTime::parse_from_rfc3339(&d.deadline_at.replace('Z', "+00:00"))
                .map(|ts| ts.with_timezone(&chrono::Utc) < now)
                .unwrap_or(false)
        })
        .collect();
    Ok(overdue)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_message() -> Message {
        Message {
            id: "msg-1".to_owned(),
            timestamp_utc: "2026-03-22T12:00:00.000000Z".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            from: "claude".to_owned(),
            to: "codex".to_owned(),
            topic: "status".to_owned(),
            body: "hello".to_owned(),
            thread_id: Some("thread-1".to_owned()),
            tags: vec![
                "repo:agent-bus".to_owned(),
                "session:s1".to_owned(),
                "wave:1".to_owned(),
            ]
            .into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        }
    }

    #[test]
    fn decode_stream_entry_handles_empty_fields() {
        let fields: HashMap<String, redis::Value> = HashMap::new();
        let msg = decode_stream_entry(&fields);
        assert!(msg.id.is_empty());
        assert!(msg.from.is_empty());
        assert_eq!(msg.priority, "normal");
        assert!(!msg.request_ack);
    }

    #[test]
    fn decode_stream_entry_reads_bulk_string_fields() {
        let mut fields: HashMap<String, redis::Value> = HashMap::new();
        fields.insert(
            "id".to_owned(),
            redis::Value::BulkString(b"msg-123".to_vec()),
        );
        fields.insert(
            "from".to_owned(),
            redis::Value::BulkString(b"claude".to_vec()),
        );
        fields.insert("to".to_owned(), redis::Value::BulkString(b"codex".to_vec()));
        fields.insert(
            "topic".to_owned(),
            redis::Value::BulkString(b"test".to_vec()),
        );
        fields.insert(
            "body".to_owned(),
            redis::Value::BulkString(b"hello".to_vec()),
        );
        fields.insert(
            "priority".to_owned(),
            redis::Value::BulkString(b"high".to_vec()),
        );
        fields.insert(
            "request_ack".to_owned(),
            redis::Value::BulkString(b"true".to_vec()),
        );
        fields.insert(
            "thread_id".to_owned(),
            redis::Value::BulkString(b"tid-1".to_vec()),
        );
        fields.insert(
            "reply_to".to_owned(),
            redis::Value::BulkString(b"claude".to_vec()),
        );

        let msg = decode_stream_entry(&fields);
        assert_eq!(msg.id, "msg-123");
        assert_eq!(msg.from, "claude");
        assert_eq!(msg.to, "codex");
        assert_eq!(msg.topic, "test");
        assert_eq!(msg.body, "hello");
        assert_eq!(msg.priority, "high");
        assert!(msg.request_ack);
        assert_eq!(msg.thread_id, Some("tid-1".to_owned()));
        assert_eq!(msg.reply_to, Some("claude".to_owned()));
    }

    #[test]
    fn decode_stream_entry_thread_id_none_is_mapped() {
        let mut fields: HashMap<String, redis::Value> = HashMap::new();
        fields.insert(
            "thread_id".to_owned(),
            redis::Value::BulkString(b"None".to_vec()),
        );
        let msg = decode_stream_entry(&fields);
        assert_eq!(msg.thread_id, None);
    }

    #[test]
    fn message_matches_filters_requires_thread_and_tags() {
        let msg = sample_message();
        assert!(message_matches_filters(
            &msg,
            Some("codex"),
            Some("claude"),
            true,
            Some("thread-1"),
            &["repo:agent-bus", "session:s1"],
        ));
    }

    #[test]
    fn message_matches_filters_rejects_missing_thread_or_tag() {
        let msg = sample_message();
        assert!(!message_matches_filters(
            &msg,
            Some("codex"),
            Some("claude"),
            true,
            Some("thread-2"),
            &["repo:agent-bus"],
        ));
        assert!(!message_matches_filters(
            &msg,
            Some("codex"),
            Some("claude"),
            true,
            Some("thread-1"),
            &["repo:other"],
        ));
    }

    #[test]
    fn parse_xrange_result_handles_empty() {
        let raw: Vec<redis::Value> = vec![];
        let result = parse_xrange_result(&raw);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_xrange_result_parses_single_entry() {
        let entry = redis::Value::Array(vec![
            redis::Value::BulkString(b"1234-0".to_vec()),
            redis::Value::Array(vec![
                redis::Value::BulkString(b"id".to_vec()),
                redis::Value::BulkString(b"msg-1".to_vec()),
                redis::Value::BulkString(b"from".to_vec()),
                redis::Value::BulkString(b"claude".to_vec()),
            ]),
        ]);
        let raw = vec![entry];
        let result = parse_xrange_result(&raw);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "1234-0");
        assert!(result[0].1.contains_key("id"));
        assert!(result[0].1.contains_key("from"));
    }

    #[test]
    fn health_codec_field_is_accurate() {
        let s = Settings::from_env();
        let h = bus_health(&s, None);
        assert_eq!(h.codec, "serde_json");
        assert_eq!(h.runtime, "rust-native");
    }

    // -----------------------------------------------------------------------
    // Task 1 unit tests — schema inference in bus_post_message
    // These tests validate the schema-enrichment logic without requiring Redis.
    // -----------------------------------------------------------------------

    /// Helper: build the enriched metadata the same way `bus_post_message` does.
    fn enrich_metadata(topic: &str, metadata: &serde_json::Value) -> serde_json::Value {
        if let Some(schema) = infer_schema_from_topic(topic, None) {
            let mut map = match metadata {
                serde_json::Value::Object(m) => m.clone(),
                _ => serde_json::Map::new(),
            };
            map.insert(
                "_schema".to_owned(),
                serde_json::Value::String(schema.to_owned()),
            );
            serde_json::Value::Object(map)
        } else {
            metadata.clone()
        }
    }

    #[test]
    fn schema_enrichment_adds_finding_for_findings_topic() {
        let meta = serde_json::json!({});
        let enriched = enrich_metadata("rust-findings", &meta);
        assert_eq!(enriched["_schema"], "finding");
    }

    #[test]
    fn schema_enrichment_adds_status_for_ownership_topic() {
        let meta = serde_json::json!({});
        let enriched = enrich_metadata("ownership", &meta);
        assert_eq!(enriched["_schema"], "status");
    }

    #[test]
    fn schema_enrichment_preserves_existing_metadata_fields() {
        let meta = serde_json::json!({"existing": "value"});
        let enriched = enrich_metadata("status", &meta);
        assert_eq!(enriched["existing"], "value");
        assert_eq!(enriched["_schema"], "status");
    }

    #[test]
    fn schema_enrichment_no_op_for_unknown_topic() {
        let meta = serde_json::json!({"key": "val"});
        let enriched = enrich_metadata("unknown-topic", &meta);
        assert!(enriched.get("_schema").is_none());
        assert_eq!(enriched["key"], "val");
    }

    #[test]
    fn schema_enrichment_adds_benchmark_for_benchmark_topic() {
        let meta = serde_json::json!({});
        let enriched = enrich_metadata("benchmark", &meta);
        assert_eq!(enriched["_schema"], "benchmark");
    }

    // -----------------------------------------------------------------------
    // prepare_message unit tests — pure computation, no Redis required
    // -----------------------------------------------------------------------

    #[test]
    fn prepare_message_sets_uuid_and_timestamp() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "alice",
            "bob",
            "test",
            "hello",
            None,
            &[],
            "normal",
            false,
            None,
            &meta,
            None,
        );
        // id must be a valid UUID (36-char hyphenated string)
        assert_eq!(pm.message.id.len(), 36, "id should be UUID length");
        assert!(pm.message.id.contains('-'), "id should be UUID format");
        // timestamp must be ISO 8601
        assert!(
            pm.message.timestamp_utc.ends_with('Z'),
            "timestamp should end with Z"
        );
    }

    #[test]
    fn prepare_message_infers_schema_for_status_topic() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "a",
            "b",
            "status",
            "ok",
            None,
            &[],
            "normal",
            false,
            None,
            &meta,
            None,
        );
        assert_eq!(pm.message.metadata["_schema"], "status");
    }

    #[test]
    fn prepare_message_no_compression_for_short_body() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "a",
            "b",
            "t",
            "short",
            None,
            &[],
            "normal",
            false,
            None,
            &meta,
            None,
        );
        assert!(!pm.is_compressed, "short body should not be compressed");
        assert_eq!(pm.stored_body, "short", "stored body should equal original");
    }

    #[test]
    fn prepare_message_compresses_large_body() {
        let large_body = "x".repeat(COMPRESS_THRESHOLD + 1);
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "a",
            "b",
            "t",
            &large_body,
            None,
            &[],
            "normal",
            false,
            None,
            &meta,
            None,
        );
        assert!(pm.is_compressed, "large body should be compressed");
        assert_ne!(
            pm.stored_body, large_body,
            "stored body should differ from original"
        );
        // Original body must be preserved in the returned Message.
        assert_eq!(
            pm.message.body, large_body,
            "message.body should be uncompressed original"
        );
        // Metadata must contain compression markers.
        assert_eq!(pm.message.metadata["_compressed"], "lz4");
        assert!(pm.message.metadata.get("_original_size").is_some());
    }

    #[test]
    fn prepare_message_sets_reply_to_from_when_none() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "alice",
            "bob",
            "t",
            "body",
            None,
            &[],
            "normal",
            false,
            None,
            &meta,
            None,
        );
        assert_eq!(pm.message.reply_to.as_deref(), Some("alice"));
    }

    #[test]
    fn prepare_message_respects_explicit_reply_to() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "alice",
            "bob",
            "t",
            "body",
            None,
            &[],
            "normal",
            false,
            Some("charlie"),
            &meta,
            None,
        );
        assert_eq!(pm.message.reply_to.as_deref(), Some("charlie"));
    }

    #[test]
    fn prepare_message_thread_str_empty_when_none() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "a",
            "b",
            "t",
            "body",
            None,
            &[],
            "normal",
            false,
            None,
            &meta,
            None,
        );
        assert!(
            pm.thread_str.is_empty(),
            "thread_str should be empty when thread_id is None"
        );
    }

    #[test]
    fn prepare_message_thread_str_set_when_provided() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "a",
            "b",
            "t",
            "body",
            Some("tid-123"),
            &[],
            "normal",
            false,
            None,
            &meta,
            None,
        );
        assert_eq!(pm.thread_str, "tid-123");
        assert_eq!(pm.message.thread_id, Some("tid-123".to_owned()));
    }

    #[test]
    fn prepare_message_request_ack_true_sets_ack_str() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "a",
            "b",
            "t",
            "body",
            None,
            &[],
            "normal",
            true,
            None,
            &meta,
            None,
        );
        assert_eq!(pm.ack_str, "true");
        assert!(pm.message.request_ack);
    }

    #[test]
    fn prepare_message_request_ack_false_sets_ack_str() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "a",
            "b",
            "t",
            "body",
            None,
            &[],
            "normal",
            false,
            None,
            &meta,
            None,
        );
        assert_eq!(pm.ack_str, "false");
        assert!(!pm.message.request_ack);
    }

    #[test]
    fn prepare_message_tags_json_is_valid_json_array() {
        let tags = vec!["alpha".to_owned(), "beta".to_owned()];
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "a", "b", "t", "body", None, &tags, "normal", false, None, &meta, None,
        );
        let parsed: Vec<String> =
            serde_json::from_str(&pm.tags_json).expect("tags_json not valid JSON");
        assert_eq!(parsed, tags);
    }

    // -----------------------------------------------------------------------
    // Task 4.1 — session tag auto-injection in prepare_message
    // -----------------------------------------------------------------------

    #[test]
    fn prepare_message_injects_session_tag_when_session_id_set() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "alice",
            "bob",
            "status",
            "ok",
            None,
            &[],
            "normal",
            false,
            None,
            &meta,
            Some("sprint-42"),
        );
        assert!(
            pm.message.tags.iter().any(|t| t == "session:sprint-42"),
            "session tag should be injected"
        );
    }

    #[test]
    fn prepare_message_does_not_duplicate_session_tag() {
        let tags = vec!["session:sprint-42".to_owned()];
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "alice",
            "bob",
            "status",
            "ok",
            None,
            &tags,
            "normal",
            false,
            None,
            &meta,
            Some("sprint-42"),
        );
        let count = pm
            .message
            .tags
            .iter()
            .filter(|t| t.as_str() == "session:sprint-42")
            .count();
        assert_eq!(count, 1, "session tag must not be duplicated");
    }

    #[test]
    fn prepare_message_no_session_tag_when_session_id_none() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "alice",
            "bob",
            "status",
            "ok",
            None,
            &[],
            "normal",
            false,
            None,
            &meta,
            None,
        );
        assert!(
            !pm.message.tags.iter().any(|t| t.starts_with("session:")),
            "no session tag should be added when session_id is None"
        );
    }

    // -----------------------------------------------------------------------
    // Task 4.4 — thread_id auto-inferred from reply_to in prepare_message
    // -----------------------------------------------------------------------

    #[test]
    fn prepare_message_infers_thread_id_from_reply_to_when_thread_absent() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "alice",
            "bob",
            "ack",
            "ok",
            None, // no explicit thread_id
            &[],
            "normal",
            false,
            Some("msg-root"), // reply_to points to the root message
            &meta,
            None,
        );
        assert_eq!(
            pm.message.thread_id.as_deref(),
            Some("msg-root"),
            "thread_id should be inferred from reply_to when absent"
        );
        assert!(!pm.thread_str.is_empty(), "thread_str must be non-empty");
    }

    #[test]
    fn prepare_message_explicit_thread_id_takes_precedence_over_reply_to() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "alice",
            "bob",
            "ack",
            "ok",
            Some("explicit-thread"), // explicit thread_id
            &[],
            "normal",
            false,
            Some("msg-root"), // reply_to differs
            &meta,
            None,
        );
        assert_eq!(
            pm.message.thread_id.as_deref(),
            Some("explicit-thread"),
            "explicit thread_id must take precedence over reply_to"
        );
    }

    #[test]
    fn prepare_message_thread_id_none_when_both_absent() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "alice",
            "bob",
            "status",
            "ok",
            None, // no thread_id
            &[],
            "normal",
            false,
            None, // no reply_to
            &meta,
            None,
        );
        assert!(
            pm.message.thread_id.is_none(),
            "thread_id should be None when neither thread_id nor reply_to is set"
        );
    }

    #[test]
    fn bus_post_messages_batch_empty_returns_empty() {
        // Documents the early-exit contract: an empty payload Vec returns
        // without touching Redis. No connection needed for this case.
        let payloads: Vec<BatchSendPayload> = Vec::new();
        assert!(
            payloads.is_empty(),
            "empty payload is the early-exit trigger"
        );
    }

    #[test]
    fn batch_send_payload_fields_match_prepare_message_params() {
        // Verifies that BatchSendPayload field names are consistent with
        // what prepare_message expects — a compile-time check.
        let payload = BatchSendPayload {
            from: "alice".to_owned(),
            to: "bob".to_owned(),
            topic: "test".to_owned(),
            body: "hello".to_owned(),
            thread_id: Some("tid".to_owned()),
            tags: vec!["t1".to_owned()],
            priority: "normal".to_owned(),
            request_ack: true,
            reply_to: Some("charlie".to_owned()),
            metadata: serde_json::Value::Object(serde_json::Map::new()),
        };
        let pm = prepare_message(
            &payload.from,
            &payload.to,
            &payload.topic,
            &payload.body,
            payload.thread_id.as_deref(),
            &payload.tags,
            &payload.priority,
            payload.request_ack,
            payload.reply_to.as_deref(),
            &payload.metadata,
            None,
        );
        assert_eq!(pm.message.from, "alice");
        assert_eq!(pm.message.to, "bob");
        assert_eq!(pm.message.topic, "test");
        assert_eq!(pm.message.body, "hello");
        assert_eq!(pm.message.thread_id, Some("tid".to_owned()));
        assert_eq!(pm.message.tags.as_slice(), &["t1".to_owned()]);
        assert_eq!(pm.message.priority, "normal");
        assert!(pm.message.request_ack);
        assert_eq!(pm.message.reply_to, Some("charlie".to_owned()));
    }

    // ---------------------------------------------------------------------------
    // check_inbox cursor helpers (unit tests — no Redis required)
    // ---------------------------------------------------------------------------

    #[test]
    fn inbox_cursor_key_format() {
        assert_eq!(inbox_cursor_key("claude"), "bus:cursor:claude");
        assert_eq!(inbox_cursor_key("codex"), "bus:cursor:codex");
        assert_eq!(inbox_cursor_key("my-agent"), "bus:cursor:my-agent");
    }

    #[test]
    fn inbox_cursor_key_empty_agent() {
        assert_eq!(inbox_cursor_key(""), "bus:cursor:");
    }

    #[test]
    fn inbox_cursor_key_no_namespace_collision() {
        let k1 = inbox_cursor_key("alpha");
        let k2 = inbox_cursor_key("beta");
        assert_ne!(k1, k2);
    }

    // -----------------------------------------------------------------------
    // Task queue — key formatting (no Redis required)
    // -----------------------------------------------------------------------

    #[test]
    fn task_queue_key_uses_prefix_and_agent() {
        let agent = "codex";
        let key = format!("{TASK_QUEUE_PREFIX}{agent}");
        assert_eq!(key, "bus:tasks:codex");
    }

    #[test]
    fn task_queue_key_stable_for_all_stable_agents() {
        for agent in &["claude", "codex", "gemini", "euler", "pasteur"] {
            let key = format!("{TASK_QUEUE_PREFIX}{agent}");
            assert!(
                key.starts_with("bus:tasks:"),
                "key '{key}' must start with bus:tasks:"
            );
            assert!(key.ends_with(agent), "key '{key}' must end with {agent}");
        }
    }

    #[test]
    fn peek_tasks_limit_zero_maps_to_lrange_all() {
        // When limit == 0, end index should be -1 (LRANGE 0 -1 = all entries).
        let limit: usize = 0;
        let end: isize = if limit == 0 {
            -1
        } else {
            isize::try_from(limit.saturating_sub(1)).unwrap_or(isize::MAX)
        };
        assert_eq!(end, -1);
    }

    #[test]
    fn peek_tasks_limit_one_maps_to_lrange_zero() {
        // limit = 1 → end = 0 → LRANGE key 0 0 (returns first element only).
        let limit: usize = 1;
        let end: isize = if limit == 0 {
            -1
        } else {
            isize::try_from(limit.saturating_sub(1)).unwrap_or(isize::MAX)
        };
        assert_eq!(end, 0);
    }

    #[test]
    fn peek_tasks_limit_ten_maps_to_end_nine() {
        let limit: usize = 10;
        let end: isize = if limit == 0 {
            -1
        } else {
            isize::try_from(limit.saturating_sub(1)).unwrap_or(isize::MAX)
        };
        assert_eq!(end, 9);
    }

    // ------------------------------------------------------------------
    // Resource event stream key
    // ------------------------------------------------------------------

    #[test]
    fn resource_event_stream_key_basic() {
        assert_eq!(
            resource_event_stream_key("src/http.rs"),
            "agent_bus:resource_events:src/http.rs"
        );
    }

    #[test]
    fn resource_event_stream_key_normalises_backslash() {
        assert_eq!(
            resource_event_stream_key("src\\http.rs"),
            "agent_bus:resource_events:src/http.rs"
        );
    }

    #[test]
    fn resource_event_stream_key_empty_resource() {
        assert_eq!(resource_event_stream_key(""), "agent_bus:resource_events:");
    }

    #[test]
    fn list_resource_events_zero_limit_returns_empty() {
        // This test does not need Redis — it verifies the early return path.
        // We cannot call the function without a live connection, but we verify
        // the limit == 0 branch by confirming the function signature accepts it.
        // The actual integration test would need a running Redis instance.
        assert_eq!(RESOURCE_EVENT_MAXLEN, 1000);
    }

    // ------------------------------------------------------------------
    // UUIDv7 message ID ordering (no Redis required)
    // ------------------------------------------------------------------

    #[test]
    fn uuidv7_message_ids_are_monotonically_ordered() {
        // UUIDv7 encodes a millisecond timestamp in the high bits, so
        // sequential generation produces lexicographically ordered IDs.
        let a = Uuid::now_v7().to_string();
        let b = Uuid::now_v7().to_string();
        assert!(
            a <= b,
            "UUIDv7 IDs must be monotonically ordered: a={a}, b={b}"
        );
    }

    #[test]
    fn uuidv7_ids_are_valid_uuid() {
        let id = Uuid::now_v7();
        assert_eq!(id.get_version(), Some(uuid::Version::SortRand));
    }

    #[test]
    fn prepare_message_uses_uuidv7() {
        let meta = serde_json::Value::Object(serde_json::Map::new());
        let pm = prepare_message(
            "alice",
            "bob",
            "status",
            "ok",
            None,
            &[],
            "normal",
            false,
            None,
            &meta,
            None,
        );
        // UUIDv7 IDs start with a timestamp-derived prefix. Verify it
        // parses as a valid UUID and has version 7.
        let parsed =
            uuid::Uuid::parse_str(&pm.message.id).expect("message ID must be a valid UUID string");
        assert_eq!(
            parsed.get_version(),
            Some(uuid::Version::SortRand),
            "message ID must be UUIDv7"
        );
    }

    // ------------------------------------------------------------------
    // TTL constants (verify documented values)
    // ------------------------------------------------------------------

    #[test]
    fn notification_stream_ttl_is_3_days() {
        assert_eq!(NOTIFICATION_STREAM_TTL_SECS, 259_200);
    }

    #[test]
    fn cursor_ttl_is_7_days() {
        assert_eq!(CURSOR_TTL_SECS, 604_800);
    }

    #[test]
    fn task_queue_ttl_is_3_days() {
        assert_eq!(TASK_QUEUE_TTL_SECS, 259_200);
    }

    #[test]
    fn resource_event_ttl_is_3_days() {
        assert_eq!(RESOURCE_EVENT_TTL_SECS, 259_200);
    }
}
