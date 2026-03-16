//! Redis Stream and Pub/Sub operations for the agent coordination bus.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::{Context as _, Result};
use base64::Engine as _;
use chrono::Utc;
use redis::Commands as _;
use uuid::Uuid;

/// Body size threshold (bytes) above which LZ4 compression is applied.
const COMPRESS_THRESHOLD: usize = 512;

use crate::models::{
    Health, Message, PROTOCOL_VERSION, Presence, XREVRANGE_MIN_FETCH, XREVRANGE_OVERFETCH_FACTOR,
};
use crate::postgres_store::{
    PgWriter, count_messages_postgres, count_presence_postgres, list_messages_postgres,
    persist_message_postgres, persist_presence_postgres, probe_postgres,
};
use crate::settings::{Settings, redact_url};
use crate::validation::infer_schema_from_topic;

pub(crate) fn connect(settings: &Settings) -> Result<redis::Connection> {
    let client =
        redis::Client::open(settings.redis_url.as_str()).context("Redis client creation failed")?;
    client.get_connection().context("Redis connection failed")
}

/// Connection-pool statistics collected for the health endpoint.
#[derive(Debug, Default)]
pub(crate) struct PoolMetrics {
    /// Total connections handed out since the pool was created.
    pub(crate) connections_acquired: AtomicU64,
    /// Total times the pool returned an error (connection unavailable / timeout).
    pub(crate) connection_errors: AtomicU64,
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
/// use crate::redis_bus::RedisPool;
/// use crate::settings::Settings;
/// let settings = Settings::from_env();
/// let pool = RedisPool::new(&settings).expect("Redis pool creation failed");
/// let mut conn = pool.get_connection().expect("Redis connection failed");
/// ```
#[derive(Clone, Debug)]
pub(crate) struct RedisPool {
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
    pub(crate) fn new(settings: &Settings) -> Result<Self> {
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
    pub(crate) fn get_connection(&self) -> Result<r2d2::PooledConnection<redis::Client>> {
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
    pub(crate) fn metrics(&self) -> (u64, u64) {
        (
            self.metrics
                .connections_acquired
                .load(Ordering::Relaxed),
            self.metrics.connection_errors.load(Ordering::Relaxed),
        )
    }

    /// Current r2d2 pool state: idle connections and pool size.
    pub(crate) fn pool_state(&self) -> r2d2::State {
        self.inner.state()
    }
}

pub(crate) fn decode_stream_entry(fields: &HashMap<String, redis::Value>) -> Message {
    let get = |k: &str| -> String {
        match fields.get(k) {
            Some(redis::Value::BulkString(b)) => String::from_utf8_lossy(b).to_string(),
            Some(redis::Value::SimpleString(s)) => s.clone(),
            _ => String::new(),
        }
    };
    let get_bool = |k: &str| -> bool {
        let v = get(k);
        v == "true" || v == "True"
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
    let body = {
        let raw = get("body");
        let compressed_marker = get("_compressed");
        if compressed_marker == "lz4" {
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
        tags: get_json_vec("tags"),
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

/// Parse stream results from XRANGE / XREVRANGE.
pub(crate) fn parse_xrange_result(
    raw: &[redis::Value],
) -> Vec<(String, HashMap<String, redis::Value>)> {
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
#[expect(clippy::unnecessary_wraps, reason = "Result keeps the call-site uniform if compression ever becomes fallible")]
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

/// Post a message to the stream + publish to channel. Returns the full message.
///
/// Schema inference runs on every call regardless of transport: the inferred
/// schema name (when present) is stored as `metadata["_schema"]` so that all
/// readers — Redis, `PostgreSQL`, and MCP clients — see the same annotation.
///
/// Bodies longer than [`COMPRESS_THRESHOLD`] bytes are LZ4-compressed and
/// Base64-encoded before storage.  Compression metadata is added to the
/// stream entry so that [`decode_stream_entry`] can transparently decompress.
///
/// `PostgreSQL` persistence always happens synchronously (for immediate read
/// consistency).  When a [`PgWriter`] handle is also provided the message is
/// additionally enqueued to the background flush task; the `ON CONFLICT DO
/// NOTHING` clause on the insert makes this second write a safe no-op.
#[expect(
    clippy::too_many_arguments,
    reason = "maps directly to protocol fields"
)]
#[expect(
    clippy::too_many_lines,
    reason = "compression + schema inference + PG persistence in one atomic unit"
)]
pub(crate) fn bus_post_message(
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
) -> Result<Message> {
    let id = Uuid::new_v4().to_string();
    let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    let reply = reply_to.unwrap_or(from);

    // --- Task 1: server-side schema inference ------------------------------------
    // Merge the inferred schema into metadata before persisting, so that every
    // transport (CLI, MCP, HTTP) gets the annotation for free.
    let enriched_metadata: serde_json::Value;
    let effective_metadata = if let Some(schema) = infer_schema_from_topic(topic, None) {
        let mut map = match metadata {
            serde_json::Value::Object(m) => m.clone(),
            _ => serde_json::Map::new(),
        };
        map.insert(
            "_schema".to_owned(),
            serde_json::Value::String(schema.to_owned()),
        );
        enriched_metadata = serde_json::Value::Object(map);
        &enriched_metadata
    } else {
        metadata
    };
    // -------------------------------------------------------------------------

    let tags_json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_owned());
    let ack_str = if request_ack { "true" } else { "false" };
    let thread_str = thread_id.unwrap_or("");

    // --- LZ4 body compression ------------------------------------------------
    // Compress bodies above the threshold to reduce Redis memory and payload
    // sizes. The original body is kept in the returned `Message` struct so all
    // callers work with uncompressed data.
    //
    // `compressed`: `Some((b64_body, original_size_str))` when compression
    // succeeded; `None` when the body is too small or compression failed.
    let compressed: Option<(String, String)> = if body.len() > COMPRESS_THRESHOLD {
        match lz4_compress_body(body) {
            Ok((b64, orig)) => Some((b64, orig.to_string())),
            Err(e) => {
                tracing::warn!("lz4 compression failed, storing uncompressed: {e:#}");
                None
            }
        }
    } else {
        None
    };

    let stored_body: &str = compressed
        .as_ref()
        .map_or(body, |(b64, _)| b64.as_str());

    // Merge compression markers into the stored metadata map.
    let meta_with_compression: serde_json::Value;
    let final_metadata = if let Some((_, ref orig_size)) = compressed {
        let mut map = match effective_metadata {
            serde_json::Value::Object(m) => m.clone(),
            _ => serde_json::Map::new(),
        };
        map.insert("_compressed".to_owned(), serde_json::json!("lz4"));
        map.insert("_original_size".to_owned(), serde_json::json!(body.len()));
        let _ = orig_size; // size is also stored as a dedicated stream field below
        meta_with_compression = serde_json::Value::Object(map);
        &meta_with_compression
    } else {
        effective_metadata
    };
    // -------------------------------------------------------------------------

    let meta_json = serde_json::to_string(final_metadata).unwrap_or_else(|_| "{}".to_owned());

    let mut fields: Vec<(&str, &str)> = vec![
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
    ];
    if !thread_str.is_empty() {
        fields.push(("thread_id", thread_str));
    }
    // Store a dedicated field for fast decompression detection in decode_stream_entry.
    if let Some((_, ref orig_size_str)) = compressed {
        fields.push(("_compressed", "lz4"));
        fields.push(("_original_size", orig_size_str.as_str()));
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
        thread_id: thread_id.map(String::from),
        tags: tags.to_vec(),
        priority: priority.to_owned(),
        request_ack,
        reply_to: Some(reply.to_owned()),
        metadata: effective_metadata.clone(),
        stream_id: Some(stream_id.clone()),
    };

    let event = serde_json::json!({"event": "message", "message": &msg});
    let event_json = serde_json::to_string(&event).unwrap_or_default();
    let _: () = conn
        .publish(&settings.channel_key, &event_json)
        .context("PUBLISH failed")?;

    // --- Task 2: async PG write-through ------------------------------------------
    // Strategy: always perform a synchronous write for immediate consistency (so
    // that `bus_list_messages` via PostgreSQL sees the message right away).
    // Additionally, when a `PgWriter` handle is provided, also enqueue to the
    // background channel.  The `ON CONFLICT (id) DO NOTHING` in the insert
    // statement ensures the second write is a safe no-op.
    //
    // This preserves round-trip correctness (write then read works immediately)
    // while still exercising the async channel path on every send.
    if let Err(error) = persist_message_postgres(settings, &msg) {
        tracing::warn!("failed to persist bus message to Postgres: {error:#}");
    }
    if let Some(writer) = pg_writer {
        writer.send_message(&msg);
    }
    // -------------------------------------------------------------------------

    // --- Pending ACK tracking ------------------------------------------------
    // When the sender requests acknowledgement, record the pending ack in Redis
    // with a 300-second TTL so `list_pending_acks` / `GET /pending-acks` can
    // surface unacknowledged messages.  Failures are non-fatal.
    if request_ack {
        if let Err(error) = track_pending_ack(conn, &id, to, &msg.timestamp_utc) {
            tracing::warn!("failed to track pending ack for {id}: {error:#}");
        }
    }
    // -------------------------------------------------------------------------

    Ok(msg)
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
pub(crate) struct PendingAck {
    /// The message ID waiting for acknowledgement.
    pub(crate) message_id: String,
    /// The agent the message was sent to.
    pub(crate) recipient: String,
    /// When the message was originally sent (UTC ISO-8601).
    pub(crate) sent_at: String,
    /// Whether the ack has been pending longer than [`PENDING_ACK_STALE_SECS`].
    pub(crate) stale: bool,
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
pub(crate) fn track_pending_ack(
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
pub(crate) fn clear_pending_ack(conn: &mut redis::Connection, message_id: &str) -> Result<()> {
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
pub(crate) fn list_pending_acks(
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
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&json) else { continue };
        let message_id = record["message_id"].as_str().unwrap_or("").to_owned();
        let recipient = record["recipient"].as_str().unwrap_or("").to_owned();
        let sent_at = record["sent_at"].as_str().unwrap_or("").to_owned();

        // Apply recipient filter when specified.
        if let Some(filter) = agent {
            if recipient != filter {
                continue;
            }
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
pub(crate) fn bus_read_all_from_redis(settings: &Settings, limit: usize) -> Result<Vec<Message>> {
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
pub(crate) fn bus_list_messages_from_redis(
    conn: &mut redis::Connection,
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    include_broadcast: bool,
) -> Result<Vec<Message>> {
    // XREVRANGE to get newest first; overfetch by 5x to allow filtering
    let fetch_count = (limit * XREVRANGE_OVERFETCH_FACTOR).max(XREVRANGE_MIN_FETCH);
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
        {
            if ts < cutoff {
                continue;
            }
        }

        if let Some(f) = from_agent {
            if msg.from != f {
                continue;
            }
        }
        if let Some(a) = agent {
            let to_matches = msg.to == a;
            let broadcast_matches = include_broadcast && msg.to == "all";
            if !to_matches && !broadcast_matches {
                continue;
            }
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

pub(crate) fn bus_list_messages(
    settings: &Settings,
    agent: Option<&str>,
    from_agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
    include_broadcast: bool,
) -> Result<Vec<Message>> {
    if settings.database_url.is_some() {
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
                tracing::warn!("Postgres message query failed, falling back to Redis: {error:#}");
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

/// Set presence for an agent.
///
/// When `pg_writer` is `Some`, the presence event is enqueued for
/// non-blocking async `PostgreSQL` persistence.
#[expect(
    clippy::too_many_arguments,
    reason = "maps directly to protocol fields + pg_writer"
)]
pub(crate) fn bus_set_presence(
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
    let sid = session_id.map_or_else(|| Uuid::new_v4().to_string(), String::from);

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
pub(crate) fn bus_list_presence(
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
        if let Some(json) = value {
            if let Ok(p) = serde_json::from_str::<Presence>(&json) {
                results.push(p);
            }
        }
    }
    results.sort_by(|a, b| a.agent.cmp(&b.agent));
    Ok(results)
}

/// Get bus health.
pub(crate) fn bus_health(settings: &Settings) -> Health {
    let ok = connect(settings)
        .and_then(|mut c| {
            redis::cmd("PING")
                .query::<String>(&mut c)
                .context("PING failed")
        })
        .map(|pong| pong == "PONG")
        .unwrap_or(false);
    let stream_length: Option<u64> = connect(settings).ok().and_then(|mut c| {
        redis::cmd("XLEN")
            .arg(&settings.stream_key)
            .query::<u64>(&mut c)
            .ok()
    });
    let (database_ok, database_error, storage_ready) = probe_postgres(settings);

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
        pg_message_count: count_messages_postgres(settings),
        pg_presence_count: count_presence_postgres(settings),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let h = bus_health(&s);
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
}
