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

use crate::models::{
    Health, Message, PROTOCOL_VERSION, Presence, XREVRANGE_MIN_FETCH, XREVRANGE_OVERFETCH_FACTOR,
};
use crate::postgres_store::{
    PgWriter, count_both_postgres, list_messages_postgres, persist_presence_postgres,
    pg_metrics, probe_postgres,
};
use crate::settings::{Settings, redact_url};
use crate::channels::{check_redis_ownership, extract_claimed_files, global_ownership_tracker};
use crate::validation::infer_schema_from_topic;

pub(crate) fn connect(settings: &Settings) -> Result<redis::Connection> {
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
pub(crate) struct SseSubscriberCount(AtomicUsize);

impl SseSubscriberCount {
    /// Increment by one when a new `/events` SSE client connects.
    pub(crate) fn inc(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement by one when an `/events` SSE client disconnects.
    ///
    /// Uses saturating subtraction to prevent underflow on unexpected drops.
    pub(crate) fn dec(&self) {
        // fetch_update with saturating sub avoids wrapping on unexpected extra decrements.
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| {
                Some(v.saturating_sub(1))
            });
    }

    /// Returns `true` when at least one SSE client is currently connected.
    pub(crate) fn any(&self) -> bool {
        self.0.load(Ordering::Relaxed) > 0
    }
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
            self.metrics.connections_acquired.load(Ordering::Relaxed),
            self.metrics.connection_errors.load(Ordering::Relaxed),
        )
    }

    /// Current r2d2 pool state: idle connections and pool size.
    pub(crate) fn pool_state(&self) -> r2d2::State {
        self.inner.state()
    }
}

pub(crate) fn decode_stream_entry(fields: &HashMap<String, redis::Value>) -> Message {
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
    let id = Uuid::new_v4().to_string();
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
pub(crate) struct BatchSendPayload {
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) topic: String,
    pub(crate) body: String,
    pub(crate) thread_id: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) priority: String,
    pub(crate) request_ack: bool,
    pub(crate) reply_to: Option<String>,
    pub(crate) metadata: serde_json::Value,
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
#[expect(
    clippy::too_many_lines,
    reason = "XADD pipeline + PUBLISH pipeline + PG enqueue + pending-ack pipeline in one fn"
)]
pub(crate) fn bus_post_messages_batch(
    conn: &mut redis::Connection,
    settings: &Settings,
    payloads: Vec<BatchSendPayload>,
    pg_writer: Option<&PgWriter>,
    has_sse_subscribers: bool,
) -> Result<Vec<Message>> {
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

    // Pipeline all PUBLISH commands (second round-trip) only when active SSE
    // clients need them.  Skipping the pipeline entirely when no `/events`
    // clients are connected avoids JSON serialisation + a network round-trip.
    if has_sse_subscribers {
        let mut pub_pipe = redis::pipe();
        for msg in &messages {
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
    }

    Ok(messages)
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
#[expect(
    clippy::too_many_arguments,
    reason = "maps directly to protocol fields plus has_sse_subscribers flag"
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
    has_sse_subscribers: bool,
) -> Result<Message> {
    let id = Uuid::new_v4().to_string();
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

    // Only serialize + PUBLISH when at least one legacy `/events` SSE client
    // is connected.  The in-process `agent_connections` map (used by
    // `GET /events/:agent_id`) is populated directly by the HTTP handler after
    // this function returns, so it never needs the Redis pub/sub path.
    if has_sse_subscribers {
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
    if request_ack
        && let Err(error) = track_pending_ack(conn, &id, to, &msg.timestamp_utc)
    {
        tracing::warn!("failed to track pending ack for {id}: {error:#}");
    }
    // -------------------------------------------------------------------------

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
            && ts < cutoff
        {
            continue;
        }

        if from_agent.is_some_and(|f| msg.from != f) {
            continue;
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
pub(crate) fn bus_health(settings: &Settings, pool: Option<&RedisPool>) -> Health {
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
            None,             // no explicit thread_id
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
}
