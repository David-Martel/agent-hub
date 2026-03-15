//! Redis Stream and Pub/Sub operations for the agent coordination bus.

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use chrono::Utc;
use redis::Commands as _;
use uuid::Uuid;

use crate::models::{
    Health, Message, PROTOCOL_VERSION, Presence, XREVRANGE_MIN_FETCH, XREVRANGE_OVERFETCH_FACTOR,
};
use crate::postgres_store::{
    count_messages_postgres, count_presence_postgres, list_messages_postgres,
    persist_message_postgres, persist_presence_postgres, probe_postgres,
};
use crate::settings::{Settings, redact_url};

pub(crate) fn connect(settings: &Settings) -> Result<redis::Connection> {
    let client =
        redis::Client::open(settings.redis_url.as_str()).context("Redis client creation failed")?;
    client.get_connection().context("Redis connection failed")
}

/// Shared Redis client for connection reuse in HTTP mode.
///
/// `redis::Client` handles reconnection internally on each `get_connection()` call,
/// so a single `RedisPool` instance can serve the entire lifetime of the HTTP server
/// without creating a new TCP connection per request.
///
/// # Examples
///
/// ```
/// let settings = Settings::from_env();
/// let pool = RedisPool::new(&settings).expect("Redis client creation failed");
/// let mut conn = pool.get_connection().expect("Redis connection failed");
/// ```
#[derive(Clone, Debug)]
pub(crate) struct RedisPool {
    client: redis::Client,
}

impl RedisPool {
    /// Create a new `RedisPool` from the given settings.
    ///
    /// # Errors
    ///
    /// Returns an error if the Redis URL is malformed.
    pub(crate) fn new(settings: &Settings) -> Result<Self> {
        let client = redis::Client::open(settings.redis_url.as_str())
            .context("Redis client creation failed")?;
        Ok(Self { client })
    }

    /// Obtain a synchronous Redis connection.
    ///
    /// `redis::Client` reconnects automatically; each call may reuse an
    /// existing TCP connection or open a new one.
    ///
    /// # Errors
    ///
    /// Returns an error if a connection to Redis cannot be established.
    pub(crate) fn get_connection(&self) -> Result<redis::Connection> {
        self.client
            .get_connection()
            .context("Redis connection failed")
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

    Message {
        id: get("id"),
        timestamp_utc: get("timestamp_utc"),
        protocol_version: get("protocol_version"),
        from: get("from"),
        to: get("to"),
        topic: get("topic"),
        body: get("body"),
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

/// Post a message to the stream + publish to channel. Returns the full message.
#[expect(
    clippy::too_many_arguments,
    reason = "maps directly to protocol fields"
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
) -> Result<Message> {
    let id = Uuid::new_v4().to_string();
    let ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    let reply = reply_to.unwrap_or(from);

    let tags_json = serde_json::to_string(tags).unwrap_or_else(|_| "[]".to_owned());
    let meta_json = serde_json::to_string(metadata).unwrap_or_else(|_| "{}".to_owned());
    let ack_str = if request_ack { "true" } else { "false" };
    let thread_str = thread_id.unwrap_or("");

    let mut fields: Vec<(&str, &str)> = vec![
        ("id", &id),
        ("timestamp_utc", &ts),
        ("protocol_version", PROTOCOL_VERSION),
        ("from", from),
        ("to", to),
        ("topic", topic),
        ("body", body),
        ("tags", &tags_json),
        ("priority", priority),
        ("request_ack", ack_str),
        ("reply_to", reply),
        ("metadata", &meta_json),
    ];
    if !thread_str.is_empty() {
        fields.push(("thread_id", thread_str));
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
        metadata: metadata.clone(),
        stream_id: Some(stream_id.clone()),
    };

    let event = serde_json::json!({"event": "message", "message": &msg});
    let event_json = serde_json::to_string(&event).unwrap_or_default();
    let _: () = conn
        .publish(&settings.channel_key, &event_json)
        .context("PUBLISH failed")?;

    if let Err(error) = persist_message_postgres(settings, &msg) {
        tracing::warn!("failed to persist bus message to Postgres: {error:#}");
    }

    Ok(msg)
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
pub(crate) fn bus_set_presence(
    conn: &mut redis::Connection,
    settings: &Settings,
    agent: &str,
    status: &str,
    session_id: Option<&str>,
    capabilities: &[String],
    ttl_seconds: u64,
    metadata: &serde_json::Value,
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

    if let Err(error) = persist_presence_postgres(settings, &presence) {
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
}
