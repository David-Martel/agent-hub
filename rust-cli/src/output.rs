//! Output formatting and encoding modes.

use clap::ValueEnum;
use serde::Serialize;

use crate::models::{Message, Presence};

#[derive(Clone, Debug, ValueEnum)]
pub(crate) enum Encoding {
    Json,
    Compact,
    Minimal,
    Human,
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
        Encoding::Human => {
            // Human falls back to compact for non-message/presence data
            println!("{}", serde_json::to_string(data).unwrap_or_default());
        }
    }
}

pub(crate) fn output_message(msg: &Message, encoding: &Encoding) {
    if matches!(encoding, Encoding::Human) {
        println!(
            "[{}] {} -> {} | {} | {} | {}",
            msg.timestamp_utc, msg.from, msg.to, msg.topic, msg.priority, msg.body,
        );
    } else {
        output(msg, encoding);
    }
}

pub(crate) fn output_messages(msgs: &[Message], encoding: &Encoding) {
    if matches!(encoding, Encoding::Human) {
        for msg in msgs {
            output_message(msg, encoding);
        }
    } else {
        output(msgs, encoding);
    }
}

pub(crate) fn output_presence(p: &Presence, encoding: &Encoding) {
    if matches!(encoding, Encoding::Human) {
        println!(
            "[{}] presence {}={} session={}",
            p.timestamp_utc, p.agent, p.status, p.session_id
        );
    } else {
        output(p, encoding);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
