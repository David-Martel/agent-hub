//! Typed task queue operations wrapping [`crate::redis_bus`].
//!
//! This module provides request structs and thin delegating functions so that
//! transport layers (CLI, HTTP, MCP) share a single typed interface rather
//! than calling [`crate::redis_bus`] task functions directly.
//!
//! All functions are pure wrappers — no business logic lives here.

use anyhow::Result;

use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Push task
// ---------------------------------------------------------------------------

/// Typed request for pushing a task onto an agent's queue.
pub(crate) struct PushTaskRequest<'a> {
    /// The target agent that will consume the task.
    pub(crate) agent: &'a str,
    /// The raw task payload string (JSON or plain text).
    pub(crate) task: &'a str,
}

/// Push a task onto the tail of an agent's Redis queue.
///
/// Returns the new queue length after the push.
///
/// # Errors
///
/// Returns an error if the Redis connection or `RPUSH` command fails.
pub(crate) fn push_task(settings: &Settings, request: &PushTaskRequest<'_>) -> Result<u64> {
    crate::redis_bus::push_task(settings, request.agent, request.task)
}

// ---------------------------------------------------------------------------
// Pull task
// ---------------------------------------------------------------------------

/// Pop and return the next task from the head of an agent's queue.
///
/// Returns `None` when the queue is empty.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LPOP` command fails.
pub(crate) fn pull_task(settings: &Settings, agent: &str) -> Result<Option<String>> {
    crate::redis_bus::pull_task(settings, agent)
}

// ---------------------------------------------------------------------------
// Peek tasks
// ---------------------------------------------------------------------------

/// Peek at up to `limit` tasks from the head of an agent's queue without
/// consuming them.
///
/// `limit = 0` returns all entries.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LRANGE` command fails.
pub(crate) fn peek_tasks(settings: &Settings, agent: &str, limit: usize) -> Result<Vec<String>> {
    crate::redis_bus::peek_tasks(settings, agent, limit)
}

// ---------------------------------------------------------------------------
// Queue length
// ---------------------------------------------------------------------------

/// Return the current number of pending tasks in an agent's queue.
///
/// # Errors
///
/// Returns an error if the Redis connection or `LLEN` command fails.
pub(crate) fn task_queue_length(settings: &Settings, agent: &str) -> Result<u64> {
    crate::redis_bus::task_queue_length(settings, agent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_task_request_fields_are_accessible() {
        let req = PushTaskRequest {
            agent: "codex",
            task: r#"{"action":"review","file":"main.rs"}"#,
        };
        assert_eq!(req.agent, "codex");
        assert_eq!(req.task, r#"{"action":"review","file":"main.rs"}"#);
    }
}
