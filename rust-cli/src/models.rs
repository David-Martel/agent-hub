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
}
