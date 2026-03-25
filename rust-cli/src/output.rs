//! Output formatting and encoding modes.

use std::fmt::Write as _;

use clap::ValueEnum;
use serde::Serialize;

use crate::models::{Health, Message, Presence};

/// Output encoding mode for CLI and HTTP responses.
///
/// # Variants
///
/// - `Json`: Pretty-printed JSON (debugging)
/// - `Compact`: Minified JSON (scripts/CI)
/// - `Minimal`: Short field names, defaults stripped (~50% fewer tokens vs JSON)
/// - `Human`: Table format for terminal reading
/// - `Toon`: Token-Optimized Object Notation — ultra-compact for LLM consumption (~70% fewer tokens vs JSON)
#[derive(Clone, Debug, ValueEnum)]
pub(crate) enum Encoding {
    Json,
    Compact,
    Minimal,
    Human,
    /// Token-Optimized Object Notation: `@from→to #topic [tags] body`
    Toon,
}

pub(crate) fn output<T: Serialize + ?Sized>(data: &T, encoding: &Encoding) {
    match encoding {
        Encoding::Json => {
            println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
        }
        Encoding::Compact => {
            println!("{}", serde_json::to_string(data).unwrap_or_default());
        }
        Encoding::Minimal => {
            let value = serde_json::to_value(data).unwrap_or_default();
            let minimized = minimize_value(&value);
            println!("{}", serde_json::to_string(&minimized).unwrap_or_default());
        }
        Encoding::Human | Encoding::Toon => {
            // Human and Toon fall back to compact for non-message/presence data
            println!("{}", serde_json::to_string(data).unwrap_or_default());
        }
    }
}

pub(crate) fn output_message(msg: &Message, encoding: &Encoding) {
    match encoding {
        Encoding::Human => {
            println!(
                "[{}] {} -> {} | {} | {} | {}",
                msg.timestamp_utc, msg.from, msg.to, msg.topic, msg.priority, msg.body,
            );
        }
        Encoding::Toon => {
            println!("{}", format_message_toon(msg));
        }
        _ => {
            output(msg, encoding);
        }
    }
}

pub(crate) fn output_messages(msgs: &[Message], encoding: &Encoding) {
    if matches!(encoding, Encoding::Human | Encoding::Toon) {
        for msg in msgs {
            output_message(msg, encoding);
        }
    } else {
        output(msgs, encoding);
    }
}

pub(crate) fn output_presence(p: &Presence, encoding: &Encoding) {
    match encoding {
        Encoding::Human => {
            println!(
                "[{}] presence {}={} session={}",
                p.timestamp_utc, p.agent, p.status, p.session_id
            );
        }
        Encoding::Toon => {
            println!("{}", format_presence_toon(p));
        }
        _ => {
            output(p, encoding);
        }
    }
}

/// Format a [`Health`] value as a single TOON line.
///
/// # Example
///
/// ```text
/// ok=true r=561 p=492 v=1.0
/// ```
pub(crate) fn format_health_toon(h: &Health) -> String {
    // Pre-size: "ok=false r=18446744073709551615 p=-9223372036854775808 v=1.0" ~ 70 chars
    let mut out = String::with_capacity(72);
    let _ = write!(out, "ok={} r=", h.ok);
    match h.stream_length {
        Some(n) => {
            let _ = write!(out, "{n}");
        }
        None => out.push('?'),
    }
    out.push_str(" p=");
    match h.pg_message_count {
        Some(n) => {
            let _ = write!(out, "{n}");
        }
        None => out.push('?'),
    }
    let _ = write!(out, " v={}", h.protocol_version);
    out
}

/// Format a [`Message`] as a single TOON line.
///
/// Format: `@from→to #topic [tag1,tag2] body-first-120-chars`
///
/// # Example
///
/// ```text
/// @claude→all #coordination [repo:agent-hub] Session announced: framework-v0.4
/// ```
pub(crate) fn format_message_toon(msg: &Message) -> String {
    // Capacity estimate: fixed prefix (~10) + from + to + topic + tags + body preview.
    // Tags total + body preview ≤ 120 + reasonable tag length. Over-estimate to 256.
    let cap = 10 + msg.from.len() + msg.to.len() + msg.topic.len() + 256;
    let mut out = String::with_capacity(cap);
    // "→" is U+2192, a 3-byte UTF-8 sequence; write it as a literal char.
    let _ = write!(out, "@{}→{} #{}", msg.from, msg.to, msg.topic);
    if !msg.tags.is_empty() {
        out.push_str(" [");
        let mut first = true;
        for tag in &msg.tags {
            if !first {
                out.push(',');
            }
            out.push_str(tag);
            first = false;
        }
        out.push(']');
    }
    out.push(' ');
    // Body preview: up to 120 Unicode scalar values, avoiding a collect() allocation.
    for (i, ch) in msg.body.chars().enumerate() {
        if i == 120 {
            break;
        }
        out.push(ch);
    }
    out
}

/// Format a [`Presence`] record as a single TOON line.
///
/// Format: `~agent status [cap1,cap2] ttl=Ns`
///
/// # Example
///
/// ```text
/// ~claude online [orchestration] ttl=7200s
/// ```
pub(crate) fn format_presence_toon(p: &Presence) -> String {
    let cap = 2 + p.agent.len() + 1 + p.status.len() + 64;
    let mut out = String::with_capacity(cap);
    let _ = write!(out, "~{} {}", p.agent, p.status);
    if !p.capabilities.is_empty() {
        out.push_str(" [");
        let mut first = true;
        for c in &p.capabilities {
            if !first {
                out.push(',');
            }
            out.push_str(c);
            first = false;
        }
        out.push(']');
    }
    let _ = write!(out, " ttl={}s", p.ttl_seconds);
    out
}

pub(crate) fn minimize_value(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map {
                match k.as_str() {
                    "protocol_version" | "stream_id" => continue,
                    "tags" if v.as_array().is_some_and(Vec::is_empty) => continue,
                    "metadata" if v.as_object().is_some_and(serde_json::Map::is_empty) => {
                        continue;
                    }
                    "thread_id" if v.is_null() => continue,
                    "request_ack" if v == &serde_json::Value::Bool(false) => continue,
                    "priority" if v.as_str() == Some("normal") => continue,
                    _ => {}
                }
                let short = match k.as_str() {
                    "timestamp_utc" => "ts",
                    "request_ack" => "ack",
                    "from" => "f",
                    "to" => "t",
                    "topic" => "tp",
                    "body" => "b",
                    "priority" => "p",
                    "reply_to" => "rt",
                    "thread_id" => "tid",
                    "tags" => "tg",
                    "metadata" => "m",
                    other => other,
                };
                result.insert(short.to_owned(), minimize_value(v));
            }
            serde_json::Value::Object(result)
        }
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(minimize_value).collect())
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// MessagePack codec
// ---------------------------------------------------------------------------

/// Encode a [`serde_json::Value`] as `MessagePack` bytes.
// allow: pub(crate) API for future HTTP/MCP binary wire protocol; currently used in tests only.
#[allow(dead_code)]
pub(crate) fn encode_msgpack(
    value: &serde_json::Value,
) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec_named(value)
}

/// Decode `MessagePack` bytes into a [`serde_json::Value`].
// allow: pub(crate) API for future HTTP/MCP binary wire protocol; currently used in tests only.
#[allow(dead_code)]
pub(crate) fn decode_msgpack(data: &[u8]) -> Result<serde_json::Value, rmp_serde::decode::Error> {
    rmp_serde::from_slice(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_message() -> Message {
        Message {
            id: "test-id".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "claude".to_owned(),
            to: "all".to_owned(),
            topic: "coordination".to_owned(),
            body: "Session announced: framework-v0.4".to_owned(),
            thread_id: None,
            tags: vec!["repo:agent-hub".to_owned()].into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        }
    }

    fn make_test_presence() -> Presence {
        Presence {
            agent: "claude".to_owned(),
            status: "online".to_owned(),
            protocol_version: "1.0".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            session_id: "session-1".to_owned(),
            capabilities: vec!["orchestration".to_owned()],
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            ttl_seconds: 7200,
        }
    }

    #[test]
    fn toon_message_format_matches_spec() {
        let msg = make_test_message();
        let toon = format_message_toon(&msg);
        assert_eq!(
            toon,
            "@claude→all #coordination [repo:agent-hub] Session announced: framework-v0.4"
        );
    }

    #[test]
    fn toon_message_no_tags() {
        let mut msg = make_test_message();
        msg.tags.clear();
        let toon = format_message_toon(&msg);
        assert_eq!(
            toon,
            "@claude→all #coordination Session announced: framework-v0.4"
        );
    }

    #[test]
    fn toon_message_body_truncated_at_120_chars() {
        let mut msg = make_test_message();
        msg.body = "x".repeat(200);
        let toon = format_message_toon(&msg);
        // Body portion = everything after "@claude→all #coordination [repo:agent-hub] "
        let body_part: String = toon
            .split_once("] ")
            .map_or(toon.as_str(), |(_, b)| b)
            .to_owned();
        assert_eq!(body_part.len(), 120);
    }

    #[test]
    fn toon_presence_format_matches_spec() {
        let p = make_test_presence();
        let toon = format_presence_toon(&p);
        assert_eq!(toon, "~claude online [orchestration] ttl=7200s");
    }

    #[test]
    fn toon_presence_no_capabilities() {
        let mut p = make_test_presence();
        p.capabilities.clear();
        let toon = format_presence_toon(&p);
        assert_eq!(toon, "~claude online ttl=7200s");
    }

    #[test]
    fn toon_health_format() {
        let h = Health {
            ok: true,
            protocol_version: "1.0".to_owned(),
            redis_url: "redis://localhost".to_owned(),
            database_url: None,
            database_ok: None,
            database_error: None,
            storage_ready: true,
            runtime: "rust-native".to_owned(),
            codec: "serde_json".to_owned(),
            stream_length: Some(561),
            pg_message_count: Some(492),
            pg_presence_count: None,
            pg_writes_queued: None,
            pg_writes_completed: None,
            pg_batches: None,
            pg_write_errors: None,
        };
        let toon = format_health_toon(&h);
        assert_eq!(toon, "ok=true r=561 p=492 v=1.0");
    }

    #[test]
    fn toon_health_unknown_counts() {
        let h = Health {
            ok: false,
            protocol_version: "1.0".to_owned(),
            redis_url: "redis://localhost".to_owned(),
            database_url: None,
            database_ok: None,
            database_error: None,
            storage_ready: false,
            runtime: "rust-native".to_owned(),
            codec: "serde_json".to_owned(),
            stream_length: None,
            pg_message_count: None,
            pg_presence_count: None,
            pg_writes_queued: None,
            pg_writes_completed: None,
            pg_batches: None,
            pg_write_errors: None,
        };
        let toon = format_health_toon(&h);
        assert_eq!(toon, "ok=false r=? p=? v=1.0");
    }

    #[test]
    fn minimize_strips_defaults_and_shortens_keys() {
        let input = serde_json::json!({
            "timestamp_utc": "2026-01-01T00:00:00Z",
            "from": "claude",
            "to": "codex",
            "topic": "test",
            "body": "hello",
            "protocol_version": "1.0",
            "stream_id": "123-0",
            "tags": [],
            "metadata": {},
            "thread_id": null,
            "request_ack": false,
            "priority": "normal"
        });
        let minimized = minimize_value(&input);
        let obj = minimized.as_object().unwrap();

        // Stripped fields should be absent
        assert!(!obj.contains_key("protocol_version"));
        assert!(!obj.contains_key("stream_id"));
        assert!(!obj.contains_key("tags"));
        assert!(!obj.contains_key("tg"));
        assert!(!obj.contains_key("metadata"));
        assert!(!obj.contains_key("m"));
        assert!(!obj.contains_key("thread_id"));
        assert!(!obj.contains_key("tid"));
        assert!(!obj.contains_key("request_ack"));
        assert!(!obj.contains_key("ack"));
        assert!(!obj.contains_key("priority"));
        assert!(!obj.contains_key("p"));

        // Shortened keys should be present
        assert_eq!(obj["ts"], "2026-01-01T00:00:00Z");
        assert_eq!(obj["f"], "claude");
        assert_eq!(obj["t"], "codex");
        assert_eq!(obj["tp"], "test");
        assert_eq!(obj["b"], "hello");
    }

    #[test]
    fn minimize_preserves_non_default_values() {
        let input = serde_json::json!({
            "priority": "high",
            "request_ack": true,
            "tags": ["important"],
            "metadata": {"key": "val"},
            "thread_id": "abc-123"
        });
        let minimized = minimize_value(&input);
        let obj = minimized.as_object().unwrap();

        assert_eq!(obj["p"], "high");
        assert_eq!(obj["ack"], true);
        assert_eq!(obj["tg"], serde_json::json!(["important"]));
        assert_eq!(obj["m"], serde_json::json!({"key": "val"}));
        assert_eq!(obj["tid"], "abc-123");
    }

    #[test]
    fn msgpack_round_trip_preserves_value() {
        let original = serde_json::json!({
            "ts": "2026-01-01T00:00:00Z",
            "f": "claude",
            "t": "codex",
            "tp": "status",
            "b": "ready",
            "tg": ["repo:agent-bus"]
        });
        let encoded = encode_msgpack(&original).expect("encode should succeed");
        let decoded = decode_msgpack(&encoded).expect("decode should succeed");
        assert_eq!(original, decoded);
    }

    #[test]
    fn msgpack_is_smaller_than_json() {
        let value = serde_json::json!({
            "from": "claude", "to": "codex", "topic": "status",
            "body": "analysis complete with findings about codebase quality",
            "tags": ["repo:test", "session:bench-123"],
            "priority": "normal", "request_ack": false
        });
        let json_bytes = serde_json::to_vec(&value).unwrap();
        let msgpack_bytes = encode_msgpack(&value).unwrap();
        assert!(
            msgpack_bytes.len() < json_bytes.len(),
            "msgpack {} >= json {}",
            msgpack_bytes.len(),
            json_bytes.len()
        );
    }
}
