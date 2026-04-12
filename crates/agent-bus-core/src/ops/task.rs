//! Typed task queue operations wrapping [`crate::redis_bus`].
//!
//! This module provides request structs and thin delegating functions so that
//! transport layers (CLI, HTTP, MCP) share a single typed interface rather
//! than calling [`crate::redis_bus`] task functions directly.
//!
//! The original `push_task`, `pull_task`, and `peek_tasks` functions are
//! retained for backward compatibility with plain-string task payloads.
//! The newer `*_card` variants work with structured [`TaskCard`] values and
//! fall back gracefully when they encounter legacy plain-string entries.

use std::fmt;
use std::str::FromStr;

use crate::error::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::settings::Settings;
use crate::validation::non_empty;

// ---------------------------------------------------------------------------
// Task priority constants
// ---------------------------------------------------------------------------

/// Valid priority values for task cards.
pub const VALID_TASK_PRIORITIES: &[&str] = &["normal", "high", "critical"];

// ---------------------------------------------------------------------------
// TaskStatus
// ---------------------------------------------------------------------------

/// Lifecycle status of a task card.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Task is waiting to be picked up.
    Pending,
    /// Task has been claimed and is being worked on.
    InProgress,
    /// Task finished successfully.
    Completed,
    /// Task failed (see `body` for details).
    Failed,
    /// Task was explicitly cancelled before completion.
    Cancelled,
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        };
        f.write_str(s)
    }
}

impl FromStr for TaskStatus {
    type Err = crate::error::AgentBusError;

    /// Parse a status string into a [`TaskStatus`].
    ///
    /// Accepts the `snake_case` forms emitted by [`Display`]:
    /// `pending`, `in_progress`, `completed`, `failed`, `cancelled`.
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "pending" => Ok(Self::Pending),
            "in_progress" => Ok(Self::InProgress),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(crate::error::AgentBusError::Internal(format!(
                "invalid task status '{other}'; expected one of: \
                 pending, in_progress, completed, failed, cancelled"
            ))),
        }
    }
}

// ---------------------------------------------------------------------------
// TaskCard
// ---------------------------------------------------------------------------

/// A structured task entry with metadata, priority, and lifecycle tracking.
///
/// Task cards are serialized to JSON before being pushed onto the Redis queue,
/// making them fully backward-compatible with the plain-string queue storage.
/// When pulling, the deserializer falls back to wrapping unrecognised strings
/// in a minimal card so that legacy entries are never lost.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskCard {
    /// Unique task identifier (UUID v4, auto-generated on creation).
    pub id: String,
    /// Repository this task relates to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// File or directory paths relevant to this task.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Priority level: `normal`, `high`, or `critical`.
    pub priority: String,
    /// IDs of tasks that must complete before this one can start.
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Message ID this task is a reply to, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Free-form tags for filtering and grouping.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Current lifecycle status.
    pub status: TaskStatus,
    /// The actual task description or payload.
    pub body: String,
    /// Agent that created this task.
    pub created_by: String,
    /// ISO 8601 timestamp of creation.
    pub created_at: String,
}

// ---------------------------------------------------------------------------
// PushTaskCardRequest
// ---------------------------------------------------------------------------

/// Typed request for pushing a validated [`TaskCard`] onto an agent's queue.
#[derive(Debug)]
pub struct PushTaskCardRequest<'a> {
    /// The target agent that will consume the task.
    pub agent: &'a str,
    /// The task description or payload body (must not be empty).
    pub body: &'a str,
    /// Agent creating this task (must not be empty).
    pub created_by: &'a str,
    /// Priority: `normal` (default), `high`, or `critical`.
    pub priority: &'a str,
    /// Repository the task relates to, if any.
    pub repo: Option<&'a str>,
    /// File or directory paths relevant to this task.
    pub paths: &'a [String],
    /// IDs of tasks that must complete before this one can start.
    pub depends_on: &'a [String],
    /// Message ID this task is a reply to, if any.
    pub reply_to: Option<&'a str>,
    /// Free-form tags for filtering and grouping.
    pub tags: &'a [String],
}

// ---------------------------------------------------------------------------
// Validated task-card priority
// ---------------------------------------------------------------------------

/// Validate a task card priority string.
///
/// Accepts `normal`, `high`, or `critical`.
///
/// # Errors
///
/// Returns an error if `priority` is not one of the valid task priority values.
fn validate_task_priority(priority: &str) -> Result<()> {
    if VALID_TASK_PRIORITIES.contains(&priority) {
        Ok(())
    } else {
        Err(crate::error::AgentBusError::Internal(format!(
            "invalid task priority '{priority}'; must be one of: {}",
            VALID_TASK_PRIORITIES.join(", "))
        ))
    }
}

// ---------------------------------------------------------------------------
// Push task (plain string — backward compat)
// ---------------------------------------------------------------------------

/// Typed request for pushing a task onto an agent's queue.
#[derive(Debug)]
pub struct PushTaskRequest<'a> {
    /// The target agent that will consume the task.
    pub agent: &'a str,
    /// The raw task payload string (JSON or plain text).
    pub task: &'a str,
}

/// Push a task onto the tail of an agent's Redis queue.
///
/// Returns the new queue length after the push.
///
/// # Errors
///
/// Returns an error if the Redis connection or `RPUSH` command fails.
pub fn push_task(settings: &Settings, request: &PushTaskRequest<'_>) -> Result<u64> {
    crate::redis_bus::push_task(settings, request.agent, request.task)
}

// ---------------------------------------------------------------------------
// Push task card (structured)
// ---------------------------------------------------------------------------

/// Validate, construct, and push a [`TaskCard`] onto an agent's Redis queue.
///
/// Auto-generates the card's `id` (UUID v4) and `created_at` (ISO 8601 UTC).
/// The card is serialized to JSON before pushing, so it is fully
/// interchangeable with the plain-string `push_task` path.
///
/// Returns the created [`TaskCard`] on success.
///
/// # Errors
///
/// Returns an error if:
/// - `body` is empty or whitespace-only
/// - `created_by` is empty or whitespace-only
/// - `agent` is empty or whitespace-only
/// - `priority` is not one of `normal`, `high`, `critical`
/// - The Redis connection or `RPUSH` command fails
pub fn push_task_card(settings: &Settings, request: &PushTaskCardRequest<'_>) -> Result<TaskCard> {
    let agent = non_empty(request.agent, "agent")?;
    let body = non_empty(request.body, "body")?;
    let created_by = non_empty(request.created_by, "created_by")?;
    validate_task_priority(request.priority)?;

    let card = TaskCard {
        id: Uuid::new_v4().to_string(),
        repo: request.repo.map(str::to_owned),
        paths: request.paths.to_vec(),
        priority: request.priority.to_owned(),
        depends_on: request.depends_on.to_vec(),
        reply_to: request.reply_to.map(str::to_owned),
        tags: request.tags.to_vec(),
        status: TaskStatus::Pending,
        body: body.to_owned(),
        created_by: created_by.to_owned(),
        created_at: Utc::now().to_rfc3339(),
    };

    let json = serde_json::to_string(&card)?;
    crate::redis_bus::push_task(settings, agent, &json)?;
    Ok(card)
}

// ---------------------------------------------------------------------------
// Pull task (plain string — backward compat)
// ---------------------------------------------------------------------------

/// Pop and return the next task from the head of an agent's queue.
///
/// Returns `None` when the queue is empty.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LPOP` command fails.
pub fn pull_task(settings: &Settings, agent: &str) -> Result<Option<String>> {
    crate::redis_bus::pull_task(settings, agent)
}

// ---------------------------------------------------------------------------
// Pull task card (structured)
// ---------------------------------------------------------------------------

/// Pop the next task from an agent's queue and return it as a [`TaskCard`].
///
/// If the raw value is a valid JSON-encoded `TaskCard`, it is deserialized
/// directly. Otherwise the raw string is wrapped in a minimal `TaskCard` with
/// `body` set to the raw value and all metadata defaulted. This guarantees
/// backward compatibility with legacy plain-string task entries.
///
/// Returns `None` when the queue is empty.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LPOP` command fails.
pub fn pull_task_card(settings: &Settings, agent: &str) -> Result<Option<TaskCard>> {
    let raw = crate::redis_bus::pull_task(settings, agent)?;
    Ok(raw.map(|s| parse_or_wrap_task_card(&s)))
}

// ---------------------------------------------------------------------------
// Peek tasks (plain string — backward compat)
// ---------------------------------------------------------------------------

/// Peek at up to `limit` tasks from the head of an agent's queue without
/// consuming them.
///
/// `limit = 0` returns all entries.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LRANGE` command fails.
pub fn peek_tasks(settings: &Settings, agent: &str, limit: usize) -> Result<Vec<String>> {
    crate::redis_bus::peek_tasks(settings, agent, limit)
}

// ---------------------------------------------------------------------------
// Peek task cards (structured)
// ---------------------------------------------------------------------------

/// Peek at up to `limit` tasks as [`TaskCard`] values without consuming them.
///
/// Each raw entry is deserialized or wrapped using the same fallback logic as
/// [`pull_task_card`].
///
/// `limit = 0` returns all entries.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LRANGE` command fails.
pub fn peek_task_cards(settings: &Settings, agent: &str, limit: usize) -> Result<Vec<TaskCard>> {
    let raw_tasks = crate::redis_bus::peek_tasks(settings, agent, limit)?;
    Ok(raw_tasks
        .iter()
        .map(|s| parse_or_wrap_task_card(s))
        .collect())
}

// ---------------------------------------------------------------------------
// Queue length (unchanged)
// ---------------------------------------------------------------------------

/// Return the current number of pending tasks in an agent's queue.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LLEN` command fails.
pub fn task_queue_length(settings: &Settings, agent: &str) -> Result<u64> {
    crate::redis_bus::task_queue_length(settings, agent)
}

// ---------------------------------------------------------------------------
// Helpers (private)
// ---------------------------------------------------------------------------

/// Try to deserialize `raw` as a JSON [`TaskCard`].  If that fails (malformed
/// JSON, missing fields, or plain text), wrap the raw string in a minimal card
/// with sensible defaults so that legacy entries are never lost.
fn parse_or_wrap_task_card(raw: &str) -> TaskCard {
    serde_json::from_str::<TaskCard>(raw).unwrap_or_else(|_| TaskCard {
        id: Uuid::new_v4().to_string(),
        repo: None,
        paths: Vec::new(),
        priority: "normal".to_owned(),
        depends_on: Vec::new(),
        reply_to: None,
        tags: Vec::new(),
        status: TaskStatus::Pending,
        body: raw.to_owned(),
        created_by: String::new(),
        created_at: Utc::now().to_rfc3339(),
    })
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -- existing plain-string request ----------------------------------------

    #[test]
    fn push_task_request_fields_are_accessible() {
        let req = PushTaskRequest {
            agent: "codex",
            task: r#"{"action":"review","file":"main.rs"}"#,
        };
        assert_eq!(req.agent, "codex");
        assert_eq!(req.task, r#"{"action":"review","file":"main.rs"}"#);
    }

    // -- TaskCard serialization round-trip ------------------------------------

    #[test]
    fn task_card_serialization_round_trip() {
        let card = TaskCard {
            id: "test-id-001".to_owned(),
            repo: Some("agent-bus".to_owned()),
            paths: vec!["src/main.rs".to_owned()],
            priority: "high".to_owned(),
            depends_on: vec!["task-000".to_owned()],
            reply_to: Some("msg-42".to_owned()),
            tags: vec!["repo:agent-bus".to_owned()],
            status: TaskStatus::Pending,
            body: "Review the main entry point".to_owned(),
            created_by: "claude".to_owned(),
            created_at: "2026-04-04T12:00:00+00:00".to_owned(),
        };

        let json = serde_json::to_string(&card).expect("serialize");
        let restored: TaskCard = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.id, card.id);
        assert_eq!(restored.repo.as_deref(), Some("agent-bus"));
        assert_eq!(restored.paths, card.paths);
        assert_eq!(restored.priority, "high");
        assert_eq!(restored.depends_on, card.depends_on);
        assert_eq!(restored.reply_to.as_deref(), Some("msg-42"));
        assert_eq!(restored.tags, card.tags);
        assert_eq!(restored.status, TaskStatus::Pending);
        assert_eq!(restored.body, card.body);
        assert_eq!(restored.created_by, "claude");
        assert_eq!(restored.created_at, card.created_at);
    }

    #[test]
    fn task_card_optional_fields_omitted_in_json() {
        let card = TaskCard {
            id: "id".to_owned(),
            repo: None,
            paths: Vec::new(),
            priority: "normal".to_owned(),
            depends_on: Vec::new(),
            reply_to: None,
            tags: Vec::new(),
            status: TaskStatus::Completed,
            body: "do a thing".to_owned(),
            created_by: "agent".to_owned(),
            created_at: "2026-01-01T00:00:00+00:00".to_owned(),
        };
        let json = serde_json::to_string(&card).expect("serialize");
        // `repo` and `reply_to` are `skip_serializing_if = "Option::is_none"`
        assert!(!json.contains("\"repo\""), "repo should be omitted: {json}");
        assert!(
            !json.contains("\"reply_to\""),
            "reply_to should be omitted: {json}"
        );
    }

    // -- TaskStatus Display ---------------------------------------------------

    #[test]
    fn task_status_display() {
        assert_eq!(TaskStatus::Pending.to_string(), "pending");
        assert_eq!(TaskStatus::InProgress.to_string(), "in_progress");
        assert_eq!(TaskStatus::Completed.to_string(), "completed");
        assert_eq!(TaskStatus::Failed.to_string(), "failed");
        assert_eq!(TaskStatus::Cancelled.to_string(), "cancelled");
    }

    // -- TaskStatus FromStr ---------------------------------------------------

    #[test]
    fn task_status_from_str_valid() {
        assert_eq!(
            TaskStatus::from_str("pending").unwrap(),
            TaskStatus::Pending
        );
        assert_eq!(
            TaskStatus::from_str("in_progress").unwrap(),
            TaskStatus::InProgress
        );
        assert_eq!(
            TaskStatus::from_str("completed").unwrap(),
            TaskStatus::Completed
        );
        assert_eq!(TaskStatus::from_str("failed").unwrap(), TaskStatus::Failed);
        assert_eq!(
            TaskStatus::from_str("cancelled").unwrap(),
            TaskStatus::Cancelled
        );
    }

    #[test]
    fn task_status_from_str_invalid() {
        let err = TaskStatus::from_str("bogus").unwrap_err();
        assert!(
            err.to_string().contains("invalid task status"),
            "error should mention 'invalid task status': {err}"
        );
    }

    #[test]
    fn task_status_display_round_trip() {
        for status in &[
            TaskStatus::Pending,
            TaskStatus::InProgress,
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
        ] {
            let s = status.to_string();
            let parsed = TaskStatus::from_str(&s).unwrap();
            assert_eq!(&parsed, status);
        }
    }

    // -- push_task_card validation --------------------------------------------

    /// Construct a [`Settings`] from the environment/config file.  This never
    /// connects to Redis — it only reads env vars and the optional config JSON.
    fn test_settings() -> Settings {
        Settings::from_env()
    }

    fn valid_push_request() -> PushTaskCardRequest<'static> {
        PushTaskCardRequest {
            agent: "codex",
            body: "review main.rs",
            created_by: "claude",
            priority: "normal",
            repo: None,
            paths: &[],
            depends_on: &[],
            reply_to: None,
            tags: &[],
        }
    }

    #[test]
    fn push_task_card_rejects_empty_body() {
        let settings = test_settings();
        let mut req = valid_push_request();
        req.body = "   ";
        let result = push_task_card(&settings, &req);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("body"), "error should mention 'body': {msg}");
    }

    #[test]
    fn push_task_card_rejects_empty_created_by() {
        let settings = test_settings();
        let mut req = valid_push_request();
        req.created_by = "";
        let result = push_task_card(&settings, &req);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("created_by"),
            "error should mention 'created_by': {msg}"
        );
    }

    #[test]
    fn push_task_card_rejects_empty_agent() {
        let settings = test_settings();
        let mut req = valid_push_request();
        req.agent = "";
        let result = push_task_card(&settings, &req);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("agent"), "error should mention 'agent': {msg}");
    }

    #[test]
    fn push_task_card_rejects_bad_priority() {
        let settings = test_settings();
        let mut req = valid_push_request();
        req.priority = "ultra";
        let result = push_task_card(&settings, &req);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("invalid task priority"),
            "error should mention 'invalid task priority': {msg}"
        );
    }

    #[test]
    fn push_task_card_accepts_all_valid_priorities() {
        for priority in VALID_TASK_PRIORITIES {
            let settings = test_settings();
            let mut req = valid_push_request();
            req.priority = priority;
            let result = push_task_card(&settings, &req);
            // Will fail on Redis connect, but should NOT fail on validation.
            if let Err(ref err) = result {
                let msg = err.to_string();
                assert!(
                    !msg.contains("invalid task priority"),
                    "priority '{priority}' should be accepted but got: {msg}"
                );
            }
        }
    }

    // -- pull_task_card fallback ----------------------------------------------

    #[test]
    fn parse_or_wrap_handles_valid_task_card_json() {
        let card = TaskCard {
            id: "abc-123".to_owned(),
            repo: Some("agent-bus".to_owned()),
            paths: vec!["foo.rs".to_owned()],
            priority: "critical".to_owned(),
            depends_on: Vec::new(),
            reply_to: None,
            tags: vec!["urgent".to_owned()],
            status: TaskStatus::InProgress,
            body: "fix the bug".to_owned(),
            created_by: "gemini".to_owned(),
            created_at: "2026-04-04T00:00:00+00:00".to_owned(),
        };
        let json = serde_json::to_string(&card).expect("serialize");
        let parsed = parse_or_wrap_task_card(&json);

        assert_eq!(parsed.id, "abc-123");
        assert_eq!(parsed.status, TaskStatus::InProgress);
        assert_eq!(parsed.body, "fix the bug");
        assert_eq!(parsed.created_by, "gemini");
    }

    #[test]
    fn parse_or_wrap_handles_plain_string_fallback() {
        let raw = "just a plain text task";
        let card = parse_or_wrap_task_card(raw);

        assert_eq!(card.body, raw);
        assert_eq!(card.status, TaskStatus::Pending);
        assert_eq!(card.priority, "normal");
        assert!(card.created_by.is_empty());
        // id should be a valid UUID
        assert!(
            uuid::Uuid::parse_str(&card.id).is_ok(),
            "fallback id should be a valid UUID: {}",
            card.id
        );
    }

    #[test]
    fn parse_or_wrap_handles_invalid_json() {
        let raw = r#"{"id": "x", "body": "incomplete"#; // malformed JSON
        let card = parse_or_wrap_task_card(raw);

        assert_eq!(card.body, raw);
        assert_eq!(card.status, TaskStatus::Pending);
    }

    #[test]
    fn parse_or_wrap_handles_json_missing_required_fields() {
        // Valid JSON but missing required TaskCard fields
        let raw = r#"{"action": "review", "file": "main.rs"}"#;
        let card = parse_or_wrap_task_card(raw);

        assert_eq!(card.body, raw);
        assert_eq!(card.status, TaskStatus::Pending);
        assert_eq!(card.priority, "normal");
    }

    // -- validate_task_priority ------------------------------------------------

    #[test]
    fn validate_task_priority_accepts_valid() {
        assert!(validate_task_priority("normal").is_ok());
        assert!(validate_task_priority("high").is_ok());
        assert!(validate_task_priority("critical").is_ok());
    }

    #[test]
    fn validate_task_priority_rejects_invalid() {
        let err = validate_task_priority("low").unwrap_err();
        assert!(
            err.to_string().contains("invalid task priority"),
            "error should mention 'invalid task priority': {err}"
        );
    }
}
