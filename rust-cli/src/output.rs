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
