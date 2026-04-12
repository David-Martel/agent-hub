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

use crate::error::Result;
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
pub enum ServerMode {
    /// Normal operation — all endpoints active.
    Running,
    /// Write endpoints temporarily blocked; reads still allowed.
    Maintenance,
    /// Graceful shutdown initiated; write endpoints blocked.
    Stopping,
}

impl ServerMode {
    /// Return the mode as a static string slice.
    #[must_use]
    pub fn as_str(self) -> &'static str {
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
pub struct ServerControlStatus {
    /// Current operational mode.
    pub mode: ServerMode,
    /// When `true`, mutating endpoints (send, push-task, etc.) are blocked.
    pub write_blocked: bool,
    /// Human-readable reason for the current mode transition, if any.
    pub reason: Option<String>,
    /// Agent or operator that triggered the transition, if provided.
    pub requested_by: Option<String>,
    /// ISO-8601 UTC timestamp of the last mode transition.
    pub changed_at_utc: String,
    /// ISO-8601 UTC timestamp of the last PG flush, if any.
    pub last_flush_at_utc: Option<String>,
    /// OS process ID of the server process.
    pub pid: u32,
    /// Agent ID the service announces itself as on the bus.
    pub service_agent_id: String,
    /// Human-readable service name used in presence announcements.
    pub service_name: String,
}

impl ServerControlStatus {
    /// Create a new `ServerControlStatus` in the [`ServerMode::Running`] state.
    #[must_use]
    pub fn new(service_agent_id: &str, service_name: &str) -> Self {
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
#[must_use]
pub fn current_timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

// ---------------------------------------------------------------------------
// Service lifecycle actions (transport-agnostic state machine)
// ---------------------------------------------------------------------------

/// A service control action that can be applied to a running server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceAction {
    /// Block write endpoints and enter maintenance mode.
    Pause,
    /// Restore write endpoints and return to running mode.
    Resume,
    /// Trigger a `PostgreSQL` write-back flush without changing mode.
    Flush,
    /// Begin graceful shutdown — block writes and signal termination.
    Stop,
}

impl ServiceAction {
    /// Return the action as a static string slice matching the wire format.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Resume => "resume",
            Self::Flush => "flush",
            Self::Stop => "stop",
        }
    }
}

/// Parse a string into a [`ServiceAction`].
///
/// The input is trimmed and compared case-sensitively against the four
/// recognised action names.
///
/// # Errors
///
/// Returns an error if the string does not match a known action.
pub fn parse_service_action(s: &str) -> Result<ServiceAction> {
    match s.trim() {
        "pause" => Ok(ServiceAction::Pause),
        "resume" => Ok(ServiceAction::Resume),
        "flush" => Ok(ServiceAction::Flush),
        "stop" => Ok(ServiceAction::Stop),
        other => {
            return Err(crate::error::AgentBusError::InvalidParams(format!("unknown service action '{other}': expected pause|resume|flush|stop")))
        }
    }
}

/// Parameters for [`apply_service_action`].
#[derive(Debug)]
pub struct ServiceActionRequest<'a> {
    /// The action to perform.
    pub action: ServiceAction,
    /// Human-readable reason for the transition.
    pub reason: Option<&'a str>,
    /// Agent or operator that requested the transition.
    pub requested_by: Option<&'a str>,
    /// Whether to record a flush timestamp (caller is responsible for the
    /// actual async flush — this only updates the bookkeeping field).
    pub flush: bool,
}

/// Apply a service lifecycle action to the given control status.
///
/// This is the transport-agnostic state machine.  It mutates `status` in
/// place and returns a JSON response payload suitable for returning to the
/// caller.
///
/// **Transport responsibilities not handled here:**
/// - Async `PostgreSQL` flush (call before/after as needed).
/// - Tokio shutdown signal dispatch (for `Stop`).
///
/// # Errors
///
/// Currently infallible but returns `Result` for forward compatibility
/// (e.g. future guard-rail checks on invalid transitions).
pub fn apply_service_action(
    status: &mut ServerControlStatus,
    request: &ServiceActionRequest<'_>,
) -> Result<serde_json::Value> {
    let now = current_timestamp();

    match request.action {
        ServiceAction::Pause => {
            status.mode = ServerMode::Maintenance;
            status.write_blocked = true;
            status.reason = request
                .reason
                .filter(|r| !r.trim().is_empty())
                .map(String::from);
            status.requested_by = request
                .requested_by
                .filter(|a| !a.trim().is_empty())
                .map(String::from);
            status.changed_at_utc = now;
            if request.flush {
                status.last_flush_at_utc = Some(status.changed_at_utc.clone());
            }
        }
        ServiceAction::Resume => {
            status.mode = ServerMode::Running;
            status.write_blocked = false;
            status.reason = None;
            status.requested_by = request
                .requested_by
                .filter(|a| !a.trim().is_empty())
                .map(String::from);
            status.changed_at_utc = now;
        }
        ServiceAction::Flush => {
            status.last_flush_at_utc = Some(now);
        }
        ServiceAction::Stop => {
            status.mode = ServerMode::Stopping;
            status.write_blocked = true;
            status.reason = request
                .reason
                .filter(|r| !r.trim().is_empty())
                .map(String::from);
            status.requested_by = request
                .requested_by
                .filter(|a| !a.trim().is_empty())
                .map(String::from);
            status.changed_at_utc = now;
            if request.flush {
                status.last_flush_at_utc = Some(status.changed_at_utc.clone());
            }
        }
    }

    let snapshot = serde_json::to_value(status)?;
    Ok(serde_json::json!({
        "ok": true,
        "action": request.action.as_str(),
        "maintenance": snapshot,
    }))
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
#[must_use]
pub fn health(settings: &Settings, pool: Option<&RedisPool>) -> Health {
    bus_health(settings, pool)
}

/// List all agents that currently have a presence record in Redis.
///
/// Results are sorted by agent name.
///
/// # Errors
///
/// Returns an error if the Redis SCAN or GET commands fail.
pub fn list_presence(conn: &mut redis::Connection, settings: &Settings) -> Result<Vec<Presence>> {
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
pub fn list_pending_acks(
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

    #[test]
    fn parse_service_action_known_names() {
        assert_eq!(parse_service_action("pause").unwrap(), ServiceAction::Pause);
        assert_eq!(
            parse_service_action("resume").unwrap(),
            ServiceAction::Resume
        );
        assert_eq!(parse_service_action("flush").unwrap(), ServiceAction::Flush);
        assert_eq!(parse_service_action("stop").unwrap(), ServiceAction::Stop);
    }

    #[test]
    fn parse_service_action_trims_whitespace() {
        assert_eq!(
            parse_service_action("  pause  ").unwrap(),
            ServiceAction::Pause
        );
    }

    #[test]
    fn parse_service_action_rejects_unknown() {
        let err = parse_service_action("restart").unwrap_err();
        assert!(err.to_string().contains("unknown service action"));
    }

    #[test]
    fn service_action_as_str() {
        assert_eq!(ServiceAction::Pause.as_str(), "pause");
        assert_eq!(ServiceAction::Resume.as_str(), "resume");
        assert_eq!(ServiceAction::Flush.as_str(), "flush");
        assert_eq!(ServiceAction::Stop.as_str(), "stop");
    }

    #[test]
    fn apply_pause_sets_maintenance_and_blocks_writes() {
        let mut status = ServerControlStatus::new("test-agent", "TestHub");
        let req = ServiceActionRequest {
            action: ServiceAction::Pause,
            reason: Some("deploy"),
            requested_by: Some("operator"),
            flush: false,
        };
        let result = apply_service_action(&mut status, &req).unwrap();
        assert_eq!(status.mode, ServerMode::Maintenance);
        assert!(status.write_blocked);
        assert_eq!(status.reason.as_deref(), Some("deploy"));
        assert_eq!(status.requested_by.as_deref(), Some("operator"));
        assert!(status.last_flush_at_utc.is_none());
        assert_eq!(result["ok"], true);
        assert_eq!(result["action"], "pause");
    }

    #[test]
    fn apply_pause_with_flush_records_timestamp() {
        let mut status = ServerControlStatus::new("test-agent", "TestHub");
        let req = ServiceActionRequest {
            action: ServiceAction::Pause,
            reason: None,
            requested_by: None,
            flush: true,
        };
        apply_service_action(&mut status, &req).unwrap();
        assert!(status.last_flush_at_utc.is_some());
        assert_eq!(
            status.last_flush_at_utc.as_deref(),
            Some(status.changed_at_utc.as_str())
        );
    }

    #[test]
    fn apply_resume_restores_running() {
        let mut status = ServerControlStatus::new("test-agent", "TestHub");
        // First pause, then resume.
        let pause = ServiceActionRequest {
            action: ServiceAction::Pause,
            reason: Some("test"),
            requested_by: None,
            flush: false,
        };
        apply_service_action(&mut status, &pause).unwrap();
        assert_eq!(status.mode, ServerMode::Maintenance);

        let resume = ServiceActionRequest {
            action: ServiceAction::Resume,
            reason: None,
            requested_by: Some("operator"),
            flush: false,
        };
        let result = apply_service_action(&mut status, &resume).unwrap();
        assert_eq!(status.mode, ServerMode::Running);
        assert!(!status.write_blocked);
        assert!(status.reason.is_none());
        assert_eq!(result["action"], "resume");
    }

    #[test]
    fn apply_flush_only_updates_flush_timestamp() {
        let mut status = ServerControlStatus::new("test-agent", "TestHub");
        let original_mode = status.mode;
        let req = ServiceActionRequest {
            action: ServiceAction::Flush,
            reason: None,
            requested_by: None,
            flush: true, // ignored — flush action always records the timestamp
        };
        apply_service_action(&mut status, &req).unwrap();
        assert_eq!(status.mode, original_mode);
        assert!(!status.write_blocked);
        assert!(status.last_flush_at_utc.is_some());
    }

    #[test]
    fn apply_stop_sets_stopping_and_blocks_writes() {
        let mut status = ServerControlStatus::new("test-agent", "TestHub");
        let req = ServiceActionRequest {
            action: ServiceAction::Stop,
            reason: Some("shutdown"),
            requested_by: Some("admin"),
            flush: true,
        };
        let result = apply_service_action(&mut status, &req).unwrap();
        assert_eq!(status.mode, ServerMode::Stopping);
        assert!(status.write_blocked);
        assert_eq!(status.reason.as_deref(), Some("shutdown"));
        assert!(status.last_flush_at_utc.is_some());
        assert_eq!(result["action"], "stop");
    }

    #[test]
    fn apply_action_ignores_blank_reason_and_requested_by() {
        let mut status = ServerControlStatus::new("test-agent", "TestHub");
        let req = ServiceActionRequest {
            action: ServiceAction::Pause,
            reason: Some("   "),
            requested_by: Some("  "),
            flush: false,
        };
        apply_service_action(&mut status, &req).unwrap();
        assert!(status.reason.is_none());
        assert!(status.requested_by.is_none());
    }
}
