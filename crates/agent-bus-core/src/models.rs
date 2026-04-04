//! Protocol models, constants, and wire types.

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

pub const PROTOCOL_VERSION: &str = "1.0";

/// Maximum time window for history queries (minutes). 10080 = 7 days.
pub const MAX_HISTORY_MINUTES: u64 = 10080;

/// Overfetch multiplier for XREVRANGE to allow for client-side filtering.
pub const XREVRANGE_OVERFETCH_FACTOR: usize = 5;

/// Minimum entries to fetch from XREVRANGE even for small limits.
pub const XREVRANGE_MIN_FETCH: usize = 200;

/// Default TTL for startup presence announcement (seconds).
pub const STARTUP_PRESENCE_TTL: u64 = 300;

/// A bus message record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub timestamp_utc: String,
    pub protocol_version: String,
    pub from: String,
    pub to: String,
    pub topic: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub tags: SmallVec<[String; 4]>,
    #[serde(default = "default_priority")]
    pub priority: String,
    #[serde(default)]
    pub request_ack: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
}

#[must_use]
pub fn default_priority() -> String {
    "normal".to_owned()
}

/// Agent presence record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Presence {
    pub agent: String,
    pub status: String,
    pub protocol_version: String,
    pub timestamp_utc: String,
    pub session_id: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub metadata: serde_json::Value,
    pub ttl_seconds: u64,
}

/// A resource lifecycle event emitted when a claim is created, renewed,
/// released, resolved, or contested.
///
/// Resource events are stored in per-resource Redis streams keyed by
/// `agent_bus:resource_events:<resource_id>` with a MAXLEN of 1000.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceEvent {
    /// The type of event that occurred.
    pub event: String,
    /// Agent that performed the action.
    pub agent: String,
    /// Resource the event applies to.
    pub resource: String,
    /// ISO-8601 timestamp when the event was recorded.
    pub timestamp: String,
    /// Redis stream ID for this event entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Thread (joinable conversation scope)
// ---------------------------------------------------------------------------

/// Status of a conversation thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// Thread is active and accepting messages.
    Open,
    /// Thread is closed — no new messages should be posted.
    Closed,
    /// Thread is archived — preserved for reference only.
    Archived,
}

impl std::fmt::Display for ThreadStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Open => write!(f, "open"),
            Self::Closed => write!(f, "closed"),
            Self::Archived => write!(f, "archived"),
        }
    }
}

impl std::str::FromStr for ThreadStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "open" => Ok(Self::Open),
            "closed" => Ok(Self::Closed),
            "archived" => Ok(Self::Archived),
            other => Err(anyhow::anyhow!(
                "invalid thread status '{other}'; expected open|closed|archived"
            )),
        }
    }
}

/// A joinable conversation thread with explicit membership.
///
/// Thread data is stored as a single JSON value in Redis keyed by
/// `agent_bus:thread:<thread_id>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    /// Unique thread identifier (user-supplied or auto-generated).
    pub thread_id: String,
    /// Agent that created the thread.
    pub created_by: String,
    /// ISO 8601 UTC timestamp of creation.
    pub created_at: String,
    /// List of agent names that are members of this thread.
    pub members: Vec<String>,
    /// Optional repository this thread relates to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Optional default topic for messages in this thread.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// Current status of the thread.
    pub status: ThreadStatus,
}

// ---------------------------------------------------------------------------
// Ack deadline tracking
// ---------------------------------------------------------------------------

/// A deadline record for a message awaiting acknowledgement.
///
/// Stored in Redis at `agent_bus:ack_deadline:<message_id>` with a TTL
/// matching the deadline duration.  When the key expires, the deadline
/// is considered missed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckDeadline {
    /// The message ID awaiting acknowledgement.
    pub message_id: String,
    /// The agent the message was sent to.
    pub recipient: String,
    /// ISO 8601 UTC timestamp when the ack is due.
    pub deadline_at: String,
    /// Escalation level: 0 = initial, 1 = reminder, 2 = escalated.
    pub escalation_level: u8,
    /// Agent the deadline was escalated to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub escalated_to: Option<String>,
}

/// Default ack deadline in seconds for each priority level.
#[must_use]
pub fn ack_deadline_seconds(priority: &str) -> u64 {
    match priority {
        "critical" | "urgent" => 60,
        "high" => 120,
        _ => 300, // normal, low, or unrecognised
    }
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// Bus health response.
#[derive(Debug, Clone, Serialize)]
pub struct Health {
    pub ok: bool,
    pub protocol_version: String,
    pub redis_url: String,
    pub database_url: Option<String>,
    pub database_ok: Option<bool>,
    pub database_error: Option<String>,
    pub storage_ready: bool,
    pub runtime: String,
    pub codec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_length: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg_message_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg_presence_count: Option<i64>,
    /// Total write requests enqueued to the async `PgWriter` since process start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg_writes_queued: Option<u64>,
    /// Total write requests successfully persisted to `PostgreSQL` since process start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg_writes_completed: Option<u64>,
    /// Total flush batch cycles completed by the background `PgWriter` task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg_batches: Option<u64>,
    /// Total write failures logged by the background `PgWriter` task.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg_write_errors: Option<u64>,
}

// ---------------------------------------------------------------------------
// Subscription models
// ---------------------------------------------------------------------------

/// Valid priority levels that can be used as a minimum threshold in
/// subscription scope filters.
pub const VALID_SUBSCRIPTION_PRIORITIES: &[&str] = &["low", "normal", "high", "urgent"];

/// An agent's registered interest in a set of message scopes.
///
/// Subscriptions are metadata records stored in Redis.  The bus can use them
/// to pre-filter messages destined for an agent, but the current version
/// treats them as opt-in declarations — the actual filtering integration
/// (e.g. `check_inbox`) is planned for a later phase.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    /// Auto-generated UUID v4 identifier.
    pub id: String,
    /// The agent that owns this subscription.
    pub agent: String,
    /// The scope filters this subscription covers.
    pub scopes: SubscriptionScopes,
    /// ISO 8601 UTC timestamp of creation.
    pub created_at: String,
    /// Optional time-to-live in seconds.  When set, the subscription
    /// automatically expires after this duration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_seconds: Option<u64>,
    /// ISO 8601 UTC timestamp at which this subscription expires.
    /// Computed from `created_at + ttl_seconds` when a TTL is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// Scope filters for a [`Subscription`].
///
/// All fields are optional.  A subscription matches a message when the
/// message satisfies **all** non-empty scope fields (logical AND across
/// categories, logical OR within each category's list).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SubscriptionScopes {
    /// Match messages tagged with any of these `repo:<name>` values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<String>,
    /// Match messages tagged with any of these `session:<id>` values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sessions: Vec<String>,
    /// Match messages with any of these `thread_id` values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub threads: Vec<String>,
    /// Match messages containing any of these tags (exact match).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Match messages with any of these topic values.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub topics: Vec<String>,
    /// Minimum priority threshold.  When set, only messages at this level
    /// or above match.  Priority ordering: low < normal < high < urgent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_min: Option<String>,
    /// Match messages referencing any of these resource identifiers (e.g.
    /// file paths used in ownership claims).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<String>,
}

impl SubscriptionScopes {
    /// Returns `true` if every scope field is empty / `None`.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
            && self.sessions.is_empty()
            && self.threads.is_empty()
            && self.tags.is_empty()
            && self.topics.is_empty()
            && self.priority_min.is_none()
            && self.resources.is_empty()
    }
}

/// Map a priority string to its numeric rank for comparison.
/// Returns `None` for unrecognised values.
#[must_use]
fn priority_rank(p: &str) -> Option<u8> {
    match p {
        "low" => Some(0),
        "normal" => Some(1),
        "high" => Some(2),
        "urgent" => Some(3),
        _ => None,
    }
}

/// Check whether a [`Message`] matches the scope filters of a
/// [`Subscription`].
///
/// Matching rules (AND across categories, OR within each list):
/// - **repos**: message must have a `repo:<name>` tag matching any entry.
/// - **sessions**: message must have a `session:<id>` tag matching any entry.
/// - **threads**: message `thread_id` must match any entry.
/// - **tags**: message tags must contain at least one of the listed tags.
/// - **topics**: message topic must match any entry.
/// - **`priority_min`**: message priority must be >= the threshold.
/// - **resources**: message body or tags must reference any listed resource.
///
/// Empty scope fields are ignored (always pass).
#[must_use]
pub fn message_matches_subscription(msg: &Message, sub: &Subscription) -> bool {
    let scopes = &sub.scopes;

    // repos
    if !scopes.repos.is_empty() {
        let matched = scopes.repos.iter().any(|repo| {
            let expected = format!("repo:{repo}");
            msg.tags.iter().any(|t| t == &expected)
        });
        if !matched {
            return false;
        }
    }

    // sessions
    if !scopes.sessions.is_empty() {
        let matched = scopes.sessions.iter().any(|session| {
            let expected = format!("session:{session}");
            msg.tags.iter().any(|t| t == &expected)
        });
        if !matched {
            return false;
        }
    }

    // threads
    if !scopes.threads.is_empty() {
        let matched = msg
            .thread_id
            .as_deref()
            .is_some_and(|tid| scopes.threads.iter().any(|t| t == tid));
        if !matched {
            return false;
        }
    }

    // tags
    if !scopes.tags.is_empty() {
        let matched = scopes
            .tags
            .iter()
            .any(|scope_tag| msg.tags.iter().any(|mt| mt == scope_tag));
        if !matched {
            return false;
        }
    }

    // topics
    if !scopes.topics.is_empty() && !scopes.topics.iter().any(|t| t == &msg.topic) {
        return false;
    }

    // priority_min
    if let Some(ref min_priority) = scopes.priority_min {
        let min_rank = priority_rank(min_priority).unwrap_or(0);
        let msg_rank = priority_rank(&msg.priority).unwrap_or(1);
        if msg_rank < min_rank {
            return false;
        }
    }

    // resources — check if the message body or tags reference any listed resource
    if !scopes.resources.is_empty() {
        let matched = scopes.resources.iter().any(|resource| {
            msg.body.contains(resource)
                || msg.tags.iter().any(|t| t.contains(resource))
                || msg.topic.contains(resource)
        });
        if !matched {
            return false;
        }
    }

    true
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
            tags: smallvec::smallvec!["repo:agent-bus".to_owned(), "session:s1".to_owned(),],
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
        assert!(
            !obj.contains_key("stream_length"),
            "stream_length must be absent"
        );
        assert!(
            !obj.contains_key("pg_message_count"),
            "pg_message_count must be absent"
        );
        assert!(
            !obj.contains_key("pg_presence_count"),
            "pg_presence_count must be absent"
        );
        assert!(
            !obj.contains_key("pg_writes_queued"),
            "pg_writes_queued must be absent"
        );
        assert!(
            !obj.contains_key("pg_writes_completed"),
            "pg_writes_completed must be absent"
        );
        assert!(!obj.contains_key("pg_batches"), "pg_batches must be absent");
        assert!(
            !obj.contains_key("pg_write_errors"),
            "pg_write_errors must be absent"
        );

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

    // ------------------------------------------------------------------
    // Subscription serialization
    // ------------------------------------------------------------------

    fn make_subscription() -> Subscription {
        Subscription {
            id: "sub-001".to_owned(),
            agent: "claude".to_owned(),
            scopes: SubscriptionScopes {
                repos: vec!["agent-bus".to_owned()],
                sessions: vec!["s1".to_owned()],
                threads: vec!["thread-42".to_owned()],
                tags: vec!["urgent".to_owned()],
                topics: vec!["status".to_owned()],
                priority_min: Some("high".to_owned()),
                resources: vec!["src/main.rs".to_owned()],
            },
            created_at: "2026-04-04T12:00:00Z".to_owned(),
            ttl_seconds: Some(3600),
            expires_at: Some("2026-04-04T13:00:00Z".to_owned()),
        }
    }

    #[test]
    fn subscription_serialization_round_trip() {
        let original = make_subscription();
        let json = serde_json::to_string(&original).expect("serialize failed");
        let restored: Subscription = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(restored.id, original.id);
        assert_eq!(restored.agent, original.agent);
        assert_eq!(restored.scopes, original.scopes);
        assert_eq!(restored.created_at, original.created_at);
        assert_eq!(restored.ttl_seconds, original.ttl_seconds);
        assert_eq!(restored.expires_at, original.expires_at);
    }

    #[test]
    fn subscription_defaults_on_deserialize() {
        let json = r#"{
            "id": "sub-x",
            "agent": "codex",
            "scopes": {},
            "created_at": "2026-01-01T00:00:00Z"
        }"#;
        let sub: Subscription = serde_json::from_str(json).expect("deserialize failed");
        assert!(sub.scopes.is_empty());
        assert!(sub.ttl_seconds.is_none());
        assert!(sub.expires_at.is_none());
    }

    #[test]
    fn subscription_scopes_skip_empty_fields_in_json() {
        let sub = Subscription {
            id: "sub-empty".to_owned(),
            agent: "euler".to_owned(),
            scopes: SubscriptionScopes::default(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            ttl_seconds: None,
            expires_at: None,
        };
        let json = serde_json::to_string(&sub).expect("serialize failed");
        let v: serde_json::Value = serde_json::from_str(&json).expect("parse");
        let scopes_obj = v["scopes"].as_object().expect("scopes should be object");
        assert!(
            scopes_obj.is_empty(),
            "empty scopes should serialize as empty object: {json}"
        );
        assert!(!v.as_object().unwrap().contains_key("ttl_seconds"));
        assert!(!v.as_object().unwrap().contains_key("expires_at"));
    }

    #[test]
    fn subscription_scopes_is_empty_true_for_default() {
        assert!(SubscriptionScopes::default().is_empty());
    }

    #[test]
    fn subscription_scopes_is_empty_false_when_populated() {
        let s = SubscriptionScopes {
            repos: vec!["r".to_owned()],
            ..Default::default()
        };
        assert!(!s.is_empty());
    }

    // ------------------------------------------------------------------
    // message_matches_subscription
    // ------------------------------------------------------------------

    fn make_test_msg() -> Message {
        Message {
            id: "msg-t".to_owned(),
            timestamp_utc: "2026-04-04T12:00:00Z".to_owned(),
            protocol_version: PROTOCOL_VERSION.to_owned(),
            from: "claude".to_owned(),
            to: "codex".to_owned(),
            topic: "status".to_owned(),
            body: "working on src/main.rs refactor".to_owned(),
            thread_id: Some("thread-42".to_owned()),
            tags: smallvec::smallvec![
                "repo:agent-bus".to_owned(),
                "session:s1".to_owned(),
                "urgent".to_owned(),
            ],
            priority: "high".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Null,
            stream_id: None,
        }
    }

    fn sub_with_scopes(scopes: SubscriptionScopes) -> Subscription {
        Subscription {
            id: "sub-test".to_owned(),
            agent: "codex".to_owned(),
            scopes,
            created_at: "2026-04-04T12:00:00Z".to_owned(),
            ttl_seconds: None,
            expires_at: None,
        }
    }

    #[test]
    fn matches_empty_scopes_matches_everything() {
        let msg = make_test_msg();
        let sub = sub_with_scopes(SubscriptionScopes::default());
        assert!(message_matches_subscription(&msg, &sub));
    }

    #[test]
    fn matches_repo_scope() {
        let msg = make_test_msg();
        let sub = sub_with_scopes(SubscriptionScopes {
            repos: vec!["agent-bus".to_owned()],
            ..Default::default()
        });
        assert!(message_matches_subscription(&msg, &sub));

        let sub_miss = sub_with_scopes(SubscriptionScopes {
            repos: vec!["other-repo".to_owned()],
            ..Default::default()
        });
        assert!(!message_matches_subscription(&msg, &sub_miss));
    }

    #[test]
    fn matches_session_scope() {
        let msg = make_test_msg();
        let sub = sub_with_scopes(SubscriptionScopes {
            sessions: vec!["s1".to_owned()],
            ..Default::default()
        });
        assert!(message_matches_subscription(&msg, &sub));

        let sub_miss = sub_with_scopes(SubscriptionScopes {
            sessions: vec!["s999".to_owned()],
            ..Default::default()
        });
        assert!(!message_matches_subscription(&msg, &sub_miss));
    }

    #[test]
    fn matches_thread_scope() {
        let msg = make_test_msg();
        let sub = sub_with_scopes(SubscriptionScopes {
            threads: vec!["thread-42".to_owned()],
            ..Default::default()
        });
        assert!(message_matches_subscription(&msg, &sub));

        let sub_miss = sub_with_scopes(SubscriptionScopes {
            threads: vec!["thread-999".to_owned()],
            ..Default::default()
        });
        assert!(!message_matches_subscription(&msg, &sub_miss));
    }

    #[test]
    fn matches_thread_scope_when_msg_has_no_thread() {
        let mut msg = make_test_msg();
        msg.thread_id = None;
        let sub = sub_with_scopes(SubscriptionScopes {
            threads: vec!["thread-42".to_owned()],
            ..Default::default()
        });
        assert!(!message_matches_subscription(&msg, &sub));
    }

    #[test]
    fn matches_tag_scope() {
        let msg = make_test_msg();
        let sub = sub_with_scopes(SubscriptionScopes {
            tags: vec!["urgent".to_owned()],
            ..Default::default()
        });
        assert!(message_matches_subscription(&msg, &sub));

        let sub_miss = sub_with_scopes(SubscriptionScopes {
            tags: vec!["not-a-tag".to_owned()],
            ..Default::default()
        });
        assert!(!message_matches_subscription(&msg, &sub_miss));
    }

    #[test]
    fn matches_topic_scope() {
        let msg = make_test_msg();
        let sub = sub_with_scopes(SubscriptionScopes {
            topics: vec!["status".to_owned()],
            ..Default::default()
        });
        assert!(message_matches_subscription(&msg, &sub));

        let sub_miss = sub_with_scopes(SubscriptionScopes {
            topics: vec!["review-findings".to_owned()],
            ..Default::default()
        });
        assert!(!message_matches_subscription(&msg, &sub_miss));
    }

    #[test]
    fn matches_priority_min_scope() {
        let msg = make_test_msg(); // priority = "high"

        // high >= normal => matches
        let sub_normal = sub_with_scopes(SubscriptionScopes {
            priority_min: Some("normal".to_owned()),
            ..Default::default()
        });
        assert!(message_matches_subscription(&msg, &sub_normal));

        // high >= high => matches
        let sub_high = sub_with_scopes(SubscriptionScopes {
            priority_min: Some("high".to_owned()),
            ..Default::default()
        });
        assert!(message_matches_subscription(&msg, &sub_high));

        // high < urgent => does not match
        let sub_urgent = sub_with_scopes(SubscriptionScopes {
            priority_min: Some("urgent".to_owned()),
            ..Default::default()
        });
        assert!(!message_matches_subscription(&msg, &sub_urgent));
    }

    #[test]
    fn matches_resource_scope() {
        let msg = make_test_msg(); // body contains "src/main.rs"
        let sub = sub_with_scopes(SubscriptionScopes {
            resources: vec!["src/main.rs".to_owned()],
            ..Default::default()
        });
        assert!(message_matches_subscription(&msg, &sub));

        let sub_miss = sub_with_scopes(SubscriptionScopes {
            resources: vec!["src/lib.rs".to_owned()],
            ..Default::default()
        });
        assert!(!message_matches_subscription(&msg, &sub_miss));
    }

    #[test]
    fn matches_multiple_scopes_all_must_pass() {
        let msg = make_test_msg();
        // repo matches, topic matches => overall matches
        let sub = sub_with_scopes(SubscriptionScopes {
            repos: vec!["agent-bus".to_owned()],
            topics: vec!["status".to_owned()],
            ..Default::default()
        });
        assert!(message_matches_subscription(&msg, &sub));

        // repo matches, topic does NOT match => overall does not match
        let sub_partial = sub_with_scopes(SubscriptionScopes {
            repos: vec!["agent-bus".to_owned()],
            topics: vec!["benchmark".to_owned()],
            ..Default::default()
        });
        assert!(!message_matches_subscription(&msg, &sub_partial));
    }

    #[test]
    fn matches_or_within_list() {
        let msg = make_test_msg();
        // Multiple repos: message has "repo:agent-bus", so "other" fails but "agent-bus" succeeds => OR => pass.
        let sub = sub_with_scopes(SubscriptionScopes {
            repos: vec!["other".to_owned(), "agent-bus".to_owned()],
            ..Default::default()
        });
        assert!(message_matches_subscription(&msg, &sub));
    }

    // ------------------------------------------------------------------
    // priority_rank
    // ------------------------------------------------------------------

    #[test]
    fn priority_rank_ordering() {
        assert!(priority_rank("low").unwrap() < priority_rank("normal").unwrap());
        assert!(priority_rank("normal").unwrap() < priority_rank("high").unwrap());
        assert!(priority_rank("high").unwrap() < priority_rank("urgent").unwrap());
    }

    #[test]
    fn priority_rank_unknown_returns_none() {
        assert!(priority_rank("critical").is_none());
        assert!(priority_rank("").is_none());
    }

    // ------------------------------------------------------------------
    // ResourceEvent serialization
    // ------------------------------------------------------------------

    #[test]
    fn resource_event_round_trip() {
        let event = ResourceEvent {
            event: "claimed".to_owned(),
            agent: "claude".to_owned(),
            resource: "src/http.rs".to_owned(),
            timestamp: "2026-04-01T10:00:00.000000Z".to_owned(),
            stream_id: Some("1234567890000-0".to_owned()),
        };
        let json = serde_json::to_string(&event).expect("serialize failed");
        let restored: ResourceEvent = serde_json::from_str(&json).expect("deserialize failed");
        assert_eq!(restored.event, "claimed");
        assert_eq!(restored.agent, "claude");
        assert_eq!(restored.resource, "src/http.rs");
        assert_eq!(restored.stream_id, Some("1234567890000-0".to_owned()));
    }

    #[test]
    fn resource_event_stream_id_absent_when_none() {
        let event = ResourceEvent {
            event: "released".to_owned(),
            agent: "codex".to_owned(),
            resource: "src/lib.rs".to_owned(),
            timestamp: "2026-04-01T11:00:00.000000Z".to_owned(),
            stream_id: None,
        };
        let v: serde_json::Value = serde_json::to_value(&event).expect("to_value failed");
        let obj = v.as_object().expect("should be object");
        assert!(
            !obj.contains_key("stream_id"),
            "stream_id should be absent when None"
        );
        assert_eq!(v["event"], "released");
        assert_eq!(v["agent"], "codex");
    }

    // ------------------------------------------------------------------
    // ThreadStatus
    // ------------------------------------------------------------------

    #[test]
    fn thread_status_display() {
        assert_eq!(ThreadStatus::Open.to_string(), "open");
        assert_eq!(ThreadStatus::Closed.to_string(), "closed");
        assert_eq!(ThreadStatus::Archived.to_string(), "archived");
    }

    #[test]
    fn thread_status_from_str() {
        assert_eq!("open".parse::<ThreadStatus>().unwrap(), ThreadStatus::Open);
        assert_eq!(
            "closed".parse::<ThreadStatus>().unwrap(),
            ThreadStatus::Closed
        );
        assert_eq!(
            "archived".parse::<ThreadStatus>().unwrap(),
            ThreadStatus::Archived
        );
        assert!("invalid".parse::<ThreadStatus>().is_err());
    }

    #[test]
    fn thread_status_serde_rename() {
        let open = serde_json::to_string(&ThreadStatus::Open).unwrap();
        assert_eq!(open, "\"open\"");
        let closed: ThreadStatus = serde_json::from_str("\"closed\"").unwrap();
        assert_eq!(closed, ThreadStatus::Closed);
    }

    // ------------------------------------------------------------------
    // Thread serialization
    // ------------------------------------------------------------------

    fn make_thread() -> Thread {
        Thread {
            thread_id: "thread-42".to_owned(),
            created_by: "claude".to_owned(),
            created_at: "2026-04-04T12:00:00Z".to_owned(),
            members: vec!["claude".to_owned(), "codex".to_owned()],
            repo: Some("agent-bus".to_owned()),
            topic: Some("refactor".to_owned()),
            status: ThreadStatus::Open,
        }
    }

    #[test]
    fn thread_serialization_round_trip() {
        let original = make_thread();
        let json = serde_json::to_string(&original).expect("serialize failed");
        let restored: Thread = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(restored.thread_id, original.thread_id);
        assert_eq!(restored.created_by, original.created_by);
        assert_eq!(restored.created_at, original.created_at);
        assert_eq!(restored.members, original.members);
        assert_eq!(restored.repo, original.repo);
        assert_eq!(restored.topic, original.topic);
        assert_eq!(restored.status, original.status);
    }

    #[test]
    fn thread_optional_fields_absent_when_none() {
        let thread = Thread {
            thread_id: "t-1".to_owned(),
            created_by: "euler".to_owned(),
            created_at: "2026-01-01T00:00:00Z".to_owned(),
            members: vec![],
            repo: None,
            topic: None,
            status: ThreadStatus::Closed,
        };
        let v: serde_json::Value = serde_json::to_value(&thread).expect("to_value failed");
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("repo"), "repo must be absent when None");
        assert!(!obj.contains_key("topic"), "topic must be absent when None");
        assert_eq!(v["status"], "closed");
    }

    // ------------------------------------------------------------------
    // AckDeadline
    // ------------------------------------------------------------------

    #[test]
    fn ack_deadline_serialization_round_trip() {
        let original = AckDeadline {
            message_id: "msg-100".to_owned(),
            recipient: "codex".to_owned(),
            deadline_at: "2026-04-04T12:05:00Z".to_owned(),
            escalation_level: 0,
            escalated_to: None,
        };
        let json = serde_json::to_string(&original).expect("serialize failed");
        let restored: AckDeadline = serde_json::from_str(&json).expect("deserialize failed");

        assert_eq!(restored.message_id, original.message_id);
        assert_eq!(restored.recipient, original.recipient);
        assert_eq!(restored.deadline_at, original.deadline_at);
        assert_eq!(restored.escalation_level, original.escalation_level);
        assert!(restored.escalated_to.is_none());
    }

    #[test]
    fn ack_deadline_escalated_to_absent_when_none() {
        let d = AckDeadline {
            message_id: "m".to_owned(),
            recipient: "r".to_owned(),
            deadline_at: "2026-01-01T00:00:00Z".to_owned(),
            escalation_level: 0,
            escalated_to: None,
        };
        let v: serde_json::Value = serde_json::to_value(&d).expect("to_value failed");
        assert!(
            !v.as_object().unwrap().contains_key("escalated_to"),
            "escalated_to must be absent when None"
        );
    }

    #[test]
    fn ack_deadline_escalated_to_present_when_some() {
        let d = AckDeadline {
            message_id: "m".to_owned(),
            recipient: "r".to_owned(),
            deadline_at: "2026-01-01T00:00:00Z".to_owned(),
            escalation_level: 2,
            escalated_to: Some("orchestrator".to_owned()),
        };
        let v: serde_json::Value = serde_json::to_value(&d).expect("to_value failed");
        assert_eq!(v["escalated_to"], "orchestrator");
        assert_eq!(v["escalation_level"], 2);
    }

    // ------------------------------------------------------------------
    // ack_deadline_seconds
    // ------------------------------------------------------------------

    #[test]
    fn ack_deadline_seconds_by_priority() {
        assert_eq!(ack_deadline_seconds("critical"), 60);
        assert_eq!(ack_deadline_seconds("urgent"), 60);
        assert_eq!(ack_deadline_seconds("high"), 120);
        assert_eq!(ack_deadline_seconds("normal"), 300);
        assert_eq!(ack_deadline_seconds("low"), 300);
        assert_eq!(ack_deadline_seconds("unknown"), 300);
    }
}
