//! Protocol models, constants, and wire types.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

pub(crate) const PROTOCOL_VERSION: &str = "1.0";

/// Maximum time window for history queries (minutes). 10080 = 7 days.
pub(crate) const MAX_HISTORY_MINUTES: u64 = 10080;

/// Overfetch multiplier for XREVRANGE to allow for client-side filtering.
pub(crate) const XREVRANGE_OVERFETCH_FACTOR: usize = 5;

/// Minimum entries to fetch from XREVRANGE even for small limits.
pub(crate) const XREVRANGE_MIN_FETCH: usize = 200;

/// Default TTL for startup presence announcement (seconds).
pub(crate) const STARTUP_PRESENCE_TTL: u64 = 300;

/// A bus message record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Message {
    pub(crate) id: String,
    pub(crate) timestamp_utc: String,
    pub(crate) protocol_version: String,
    pub(crate) from: String,
    pub(crate) to: String,
    pub(crate) topic: String,
    pub(crate) body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) thread_id: Option<String>,
    #[serde(default)]
    pub(crate) tags: SmallVec<[String; 4]>,
    #[serde(default = "default_priority")]
    pub(crate) priority: String,
    #[serde(default)]
    pub(crate) request_ack: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reply_to: Option<String>,
    #[serde(default)]
    pub(crate) metadata: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream_id: Option<String>,
}

pub(crate) fn default_priority() -> String {
    "normal".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_priority_is_normal() {
        assert_eq!(default_priority(), "normal");
    }

    #[test]
    fn max_history_minutes_is_one_week() {
        assert_eq!(MAX_HISTORY_MINUTES, 7 * 24 * 60);
    }

    // ------------------------------------------------------------------
    // Message serialization
    // ------------------------------------------------------------------

    fn make_full_message() -> Message {
        Message {
            id: "msg-001".to_owned(),
            timestamp_utc: "2026-03-20T12:00:00Z".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            from: "claude".to_owned(),
            to: "codex".to_owned(),
            topic: "status".to_owned(),
            body: "hello from tests".to_owned(),
            thread_id: Some("thread-42".to_owned()),
            tags: smallvec::smallvec![
                "repo:agent-bus".to_owned(),
                "session:s1".to_owned(),
            ],
            priority: "high".to_owned(),
            request_ack: true,
            reply_to: Some("msg-000".to_owned()),
            metadata: serde_json::json!({"key": "value"}),
            stream_id: Some("1234567890000-0".to_owned()),
        }
    }

    #[test]
    fn message_serialization_round_trip() {
        let original = make_full_message();
        let json = serde_json::to_string(&original).expect("serialize failed");
        let restored: Message = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(restored.id, original.id);
        assert_eq!(restored.timestamp_utc, original.timestamp_utc);
        assert_eq!(restored.protocol_version, original.protocol_version);
        assert_eq!(restored.from, original.from);
        assert_eq!(restored.to, original.to);
        assert_eq!(restored.topic, original.topic);
        assert_eq!(restored.body, original.body);
        assert_eq!(restored.thread_id, original.thread_id);
        assert_eq!(restored.tags.as_slice(), original.tags.as_slice());
        assert_eq!(restored.priority, original.priority);
        assert_eq!(restored.request_ack, original.request_ack);
        assert_eq!(restored.reply_to, original.reply_to);
        assert_eq!(restored.metadata, original.metadata);
        // stream_id is skip_serializing_if = None but here it is Some; it must survive the trip.
        assert_eq!(restored.stream_id, original.stream_id);
    }

    #[test]
    fn message_defaults_serialize_as_expected() {
        let msg = Message {
            id: "d".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            from: "euler".to_owned(),
            to: "all".to_owned(),
            topic: "ping".to_owned(),
            body: "hi".to_owned(),
            thread_id: None,
            tags: SmallVec::new(),
            priority: default_priority(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Null,
            stream_id: None,
        };

        let v: serde_json::Value = serde_json::to_value(&msg).expect("to_value failed");

        assert_eq!(v["priority"], "normal");
        assert_eq!(v["request_ack"], false);
        // thread_id, reply_to, stream_id are skip_serializing_if = Option::is_none
        assert!(!v.as_object().unwrap().contains_key("thread_id"));
        assert!(!v.as_object().unwrap().contains_key("reply_to"));
        assert!(!v.as_object().unwrap().contains_key("stream_id"));
        // tags is #[serde(default)] but still serializes as empty array when present
        assert_eq!(v["tags"], serde_json::json!([]));
    }

    #[test]
    fn message_default_priority_is_applied_on_deserialize() {
        // JSON with no "priority" key — serde should apply default_priority()
        let json = r#"{
            "id": "x",
            "timestamp_utc": "2026-01-01T00:00:00Z",
            "protocol_version": "1.0",
            "from": "gemini",
            "to": "all",
            "topic": "test",
            "body": "body"
        }"#;
        let msg: Message = serde_json::from_str(json).expect("deserialize failed");
        assert_eq!(msg.priority, "normal");
        assert!(!msg.request_ack);
        assert!(msg.thread_id.is_none());
        assert!(msg.tags.is_empty());
    }

    // ------------------------------------------------------------------
    // SmallVec<[String; 4]> — inline (≤4) and heap-spill (>4) paths
    // ------------------------------------------------------------------

    #[test]
    fn smallvec_tags_zero_entries() {
        let msg = Message {
            id: "t0".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            from: "a".to_owned(),
            to: "b".to_owned(),
            topic: "t".to_owned(),
            body: "b".to_owned(),
            thread_id: None,
            tags: SmallVec::new(),
            priority: default_priority(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Null,
            stream_id: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tags.len(), 0);
        assert!(!back.tags.spilled());
    }

    #[test]
    fn smallvec_tags_one_entry_inline() {
        let mut tags: SmallVec<[String; 4]> = SmallVec::new();
        tags.push("repo:agent-bus".to_owned());

        let msg = Message {
            id: "t1".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            from: "a".to_owned(),
            to: "b".to_owned(),
            topic: "t".to_owned(),
            body: "b".to_owned(),
            thread_id: None,
            tags,
            priority: default_priority(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Null,
            stream_id: None,
        };
        assert!(!msg.tags.spilled(), "single tag must stay inline");
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tags.as_slice(), &["repo:agent-bus"]);
    }

    #[test]
    fn smallvec_tags_four_entries_still_inline() {
        let tags: SmallVec<[String; 4]> = smallvec::smallvec![
            "a".to_owned(),
            "b".to_owned(),
            "c".to_owned(),
            "d".to_owned(),
        ];
        assert!(!tags.spilled(), "four tags must fit inline");
        let msg = Message {
            id: "t4".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            from: "a".to_owned(),
            to: "b".to_owned(),
            topic: "t".to_owned(),
            body: "b".to_owned(),
            thread_id: None,
            tags,
            priority: default_priority(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Null,
            stream_id: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tags.len(), 4);
        assert_eq!(back.tags[3], "d");
    }

    #[test]
    fn smallvec_tags_five_entries_spill_to_heap() {
        let tags: SmallVec<[String; 4]> = smallvec::smallvec![
            "tag1".to_owned(),
            "tag2".to_owned(),
            "tag3".to_owned(),
            "tag4".to_owned(),
            "tag5".to_owned(),
        ];
        assert!(tags.spilled(), "five tags must spill to heap");
        let msg = Message {
            id: "t5".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            from: "a".to_owned(),
            to: "b".to_owned(),
            topic: "t".to_owned(),
            body: "b".to_owned(),
            thread_id: None,
            tags,
            priority: default_priority(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Null,
            stream_id: None,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tags.len(), 5);
        assert_eq!(back.tags[4], "tag5");
    }

    // ------------------------------------------------------------------
    // Health serialization
    // ------------------------------------------------------------------

    fn make_health_no_pg() -> Health {
        Health {
            ok: true,
            protocol_version: PROTOCOL_VERSION.to_owned(),
            redis_url: "redis://localhost:6380/0".to_owned(),
            database_url: None,
            database_ok: None,
            database_error: None,
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

    #[test]
    fn health_serializes_pg_none_fields_as_absent() {
        let h = make_health_no_pg();
        let v: serde_json::Value = serde_json::to_value(&h).expect("to_value failed");
        let obj = v.as_object().unwrap();

        // These are skip_serializing_if = Option::is_none — must be absent when None.
        assert!(!obj.contains_key("stream_length"), "stream_length must be absent");
        assert!(!obj.contains_key("pg_message_count"), "pg_message_count must be absent");
        assert!(!obj.contains_key("pg_presence_count"), "pg_presence_count must be absent");
        assert!(!obj.contains_key("pg_writes_queued"), "pg_writes_queued must be absent");
        assert!(!obj.contains_key("pg_writes_completed"), "pg_writes_completed must be absent");
        assert!(!obj.contains_key("pg_batches"), "pg_batches must be absent");
        assert!(!obj.contains_key("pg_write_errors"), "pg_write_errors must be absent");

        // Mandatory fields must be present.
        assert_eq!(v["ok"], true);
        assert_eq!(v["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(v["storage_ready"], false);
    }

    #[test]
    fn health_serializes_pg_counts_when_some() {
        let mut h = make_health_no_pg();
        h.pg_message_count = Some(42);
        h.pg_presence_count = Some(3);
        h.stream_length = Some(1000);
        h.pg_writes_queued = Some(50);
        h.pg_writes_completed = Some(49);
        h.pg_batches = Some(10);
        h.pg_write_errors = Some(1);

        let v: serde_json::Value = serde_json::to_value(&h).expect("to_value failed");
        assert_eq!(v["pg_message_count"], 42);
        assert_eq!(v["pg_presence_count"], 3);
        assert_eq!(v["stream_length"], 1000);
        assert_eq!(v["pg_writes_queued"], 50);
        assert_eq!(v["pg_writes_completed"], 49);
        assert_eq!(v["pg_batches"], 10);
        assert_eq!(v["pg_write_errors"], 1);
    }

    // ------------------------------------------------------------------
    // Presence serialization round-trip
    // ------------------------------------------------------------------

    #[test]
    fn presence_round_trip() {
        let original = Presence {
            agent: "codex".to_owned(),
            status: "online".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            timestamp_utc: "2026-03-20T09:00:00Z".to_owned(),
            session_id: "session-abc".to_owned(),
            capabilities: vec!["mcp".to_owned(), "rust".to_owned()],
            metadata: serde_json::json!({"workspace": "agent-bus"}),
            ttl_seconds: 300,
        };

        let json = serde_json::to_string(&original).expect("serialize failed");
        let restored: Presence = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(restored.agent, original.agent);
        assert_eq!(restored.status, original.status);
        assert_eq!(restored.protocol_version, original.protocol_version);
        assert_eq!(restored.session_id, original.session_id);
        assert_eq!(restored.capabilities, original.capabilities);
        assert_eq!(restored.ttl_seconds, original.ttl_seconds);
        assert_eq!(restored.metadata, original.metadata);
    }

    #[test]
    fn presence_defaults_on_deserialize() {
        // capabilities and metadata both have #[serde(default)]
        let json = r#"{
            "agent": "gemini",
            "status": "busy",
            "protocol_version": "1.0",
            "timestamp_utc": "2026-01-01T00:00:00Z",
            "session_id": "s0",
            "ttl_seconds": 60
        }"#;
        let p: Presence = serde_json::from_str(json).expect("deserialize failed");
        assert!(p.capabilities.is_empty());
        assert_eq!(p.metadata, serde_json::Value::Null);
    }
}

/// Agent presence record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Presence {
    pub(crate) agent: String,
    pub(crate) status: String,
    pub(crate) protocol_version: String,
    pub(crate) timestamp_utc: String,
    pub(crate) session_id: String,
    #[serde(default)]
    pub(crate) capabilities: Vec<String>,
    #[serde(default)]
    pub(crate) metadata: serde_json::Value,
    pub(crate) ttl_seconds: u64,
}

/// Bus health response.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Health {
    pub(crate) ok: bool,
    pub(crate) protocol_version: String,
    pub(crate) redis_url: String,
    pub(crate) database_url: Option<String>,
    pub(crate) database_ok: Option<bool>,
    pub(crate) database_error: Option<String>,
    pub(crate) storage_ready: bool,
    pub(crate) runtime: String,
    pub(crate) codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) stream_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pg_message_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pg_presence_count: Option<i64>,
    /// Total write requests enqueued to the async `PgWriter` since process start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pg_writes_queued: Option<u64>,
    /// Total write requests successfully persisted to `PostgreSQL` since process start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pg_writes_completed: Option<u64>,
    /// Total flush batch cycles completed by the background `PgWriter` task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pg_batches: Option<u64>,
    /// Total write failures logged by the background `PgWriter` task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pg_write_errors: Option<u64>,
}
