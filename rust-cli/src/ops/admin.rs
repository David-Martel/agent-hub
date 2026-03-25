//! Admin control and health types shared across transport layers.
//!
//! This module defines transport-agnostic types for server lifecycle state and
//! health check results.  HTTP handler logic that produces specific response
//! codes or serialises these types into Axum responses lives in
//! [`crate::http`] — only the data types are here.
//!
//! # Operations
//!
//! In addition to the data types, this module exposes thin transport-agnostic
//! wrappers around the three most-commonly delegated `redis_bus` calls:
//!
//! - [`health`] — check Redis + `PostgreSQL` liveness
//! - [`list_presence`] — read all agent presence records
//! - [`list_pending_acks`] — list messages awaiting acknowledgement
//!
//! Transport modules call these wrappers instead of importing from
//! `redis_bus` directly, so that the import surface of CLI/HTTP/MCP remains
//! bounded to transport-specific concerns.

use anyhow::Result;
use chrono::Utc;

use crate::models::{Health, Presence};
use crate::redis_bus::{
    PendingAck, RedisPool, bus_health, bus_list_presence,
    list_pending_acks as redis_list_pending_acks,
};
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Server lifecycle types
// ---------------------------------------------------------------------------

/// The operational mode of the HTTP server.
#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ServerMode {
    /// Normal operation — all endpoints active.
    Running,
    /// Write endpoints temporarily blocked; reads still allowed.
    Maintenance,
    /// Graceful shutdown initiated; write endpoints blocked.
    Stopping,
}

impl ServerMode {
    /// Return the mode as a static string slice.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Maintenance => "maintenance",
            Self::Stopping => "stopping",
        }
    }
}

/// Snapshot of the server's current lifecycle control state.
///
/// Stored behind an `Arc<RwLock<ServerControlStatus>>` in the HTTP
/// `AppState` so all handlers can read or mutate it cheaply.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct ServerControlStatus {
    /// Current operational mode.
    pub(crate) mode: ServerMode,
    /// When `true`, mutating endpoints (send, push-task, etc.) are blocked.
    pub(crate) write_blocked: bool,
    /// Human-readable reason for the current mode transition, if any.
    pub(crate) reason: Option<String>,
    /// Agent or operator that triggered the transition, if provided.
    pub(crate) requested_by: Option<String>,
    /// ISO-8601 UTC timestamp of the last mode transition.
    pub(crate) changed_at_utc: String,
    /// ISO-8601 UTC timestamp of the last PG flush, if any.
    pub(crate) last_flush_at_utc: Option<String>,
    /// OS process ID of the server process.
    pub(crate) pid: u32,
    /// Agent ID the service announces itself as on the bus.
    pub(crate) service_agent_id: String,
    /// Human-readable service name used in presence announcements.
    pub(crate) service_name: String,
}

impl ServerControlStatus {
    /// Create a new `ServerControlStatus` in the [`ServerMode::Running`] state.
    pub(crate) fn new(service_agent_id: &str, service_name: &str) -> Self {
        Self {
            mode: ServerMode::Running,
            write_blocked: false,
            reason: None,
            requested_by: None,
            changed_at_utc: current_timestamp(),
            last_flush_at_utc: None,
            pid: std::process::id(),
            service_agent_id: service_agent_id.to_owned(),
            service_name: service_name.to_owned(),
        }
    }
}

/// Return the current UTC time formatted as an ISO-8601 string with
/// microsecond precision.
pub(crate) fn current_timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

// ---------------------------------------------------------------------------
// Transport-agnostic operation wrappers
// ---------------------------------------------------------------------------

/// Check Redis and `PostgreSQL` liveness.
///
/// Pass `Some(pool)` when an r2d2 [`RedisPool`] is already held by the caller
/// (HTTP server mode) to reuse a pooled connection.  Pass `None` in CLI and
/// MCP stdio mode where no pool exists — the function opens a short-lived
/// connection internally.
///
/// # Example
///
/// ```ignore
/// let h = ops::admin::health(&settings, None);
/// assert!(h.ok);
/// ```
pub(crate) fn health(settings: &Settings, pool: Option<&RedisPool>) -> Health {
    bus_health(settings, pool)
}

/// List all agents that currently have a presence record in Redis.
///
/// Results are sorted by agent name.
///
/// # Errors
///
/// Returns an error if the Redis SCAN or GET commands fail.
pub(crate) fn list_presence(
    conn: &mut redis::Connection,
    settings: &Settings,
) -> Result<Vec<Presence>> {
    bus_list_presence(conn, settings)
}

/// List all messages that are waiting for an acknowledgement.
///
/// When `agent` is `Some`, only pending acks whose recipient matches are
/// returned.  Entries older than the stale threshold are flagged with
/// `stale = true` on the returned [`PendingAck`] records.
///
/// # Errors
///
/// Returns an error if the Redis SCAN or GET commands fail.
pub(crate) fn list_pending_acks(
    conn: &mut redis::Connection,
    agent: Option<&str>,
) -> Result<Vec<PendingAck>> {
    redis_list_pending_acks(conn, agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_control_status_defaults_to_running() {
        let status = ServerControlStatus::new("agent-bus", "AgentHub");
        assert_eq!(status.mode, ServerMode::Running);
        assert!(!status.write_blocked);
        assert_eq!(status.service_agent_id, "agent-bus");
        assert_eq!(status.service_name, "AgentHub");
        assert!(status.reason.is_none());
        assert!(status.last_flush_at_utc.is_none());
        assert_eq!(status.pid, std::process::id());
    }

    #[test]
    fn server_mode_as_str_round_trips() {
        assert_eq!(ServerMode::Running.as_str(), "running");
        assert_eq!(ServerMode::Maintenance.as_str(), "maintenance");
        assert_eq!(ServerMode::Stopping.as_str(), "stopping");
    }

}
