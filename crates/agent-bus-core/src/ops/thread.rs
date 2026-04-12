//! Thread lifecycle operations wrapping [`crate::redis_bus`].
//!
//! This module provides request structs and thin delegating functions so that
//! transport layers (CLI, HTTP, MCP) share a single typed interface for
//! creating, joining, leaving, listing, and closing threads.

use crate::error::Result;
use chrono::Utc;
use uuid::Uuid;

use crate::models::{Thread, ThreadStatus};
use crate::redis_bus;
use crate::settings::Settings;
use crate::validation::non_empty;

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

/// Typed request for creating a new thread.
#[derive(Debug)]
pub struct CreateThreadRequest<'a> {
    /// Thread ID (if empty, a UUID v4 is generated).
    pub thread_id: Option<&'a str>,
    /// Agent creating the thread (required, must not be empty).
    pub created_by: &'a str,
    /// Optional repository this thread relates to.
    pub repo: Option<&'a str>,
    /// Optional default topic.
    pub topic: Option<&'a str>,
}

/// Typed request for joining or leaving a thread.
#[derive(Debug)]
pub struct ThreadMemberRequest<'a> {
    /// Thread ID (required).
    pub thread_id: &'a str,
    /// Agent joining or leaving (required).
    pub agent: &'a str,
}

// ---------------------------------------------------------------------------
// Operations
// ---------------------------------------------------------------------------

/// Create a new conversation thread.
///
/// The creator is automatically added as the first member.
///
/// # Errors
///
/// Returns an error if `created_by` is empty, if the thread already exists,
/// or if the Redis connection fails.
pub fn create_thread(settings: &Settings, request: &CreateThreadRequest<'_>) -> Result<Thread> {
    let agent = non_empty(request.created_by, "created_by")?;
    let thread_id = request
        .thread_id
        .filter(|s| !s.trim().is_empty())
        .map_or_else(|| Uuid::new_v4().to_string(), str::to_owned);

    let thread = Thread {
        thread_id,
        created_by: agent.to_owned(),
        created_at: Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
        members: vec![agent.to_owned()],
        repo: request.repo.map(str::to_owned),
        topic: request.topic.map(str::to_owned),
        status: ThreadStatus::Open,
    };

    let mut conn = redis_bus::connect(settings)?;
    redis_bus::create_thread(&mut conn, &thread)?;
    Ok(thread)
}

/// Add an agent to a thread.
///
/// # Errors
///
/// Returns an error if the thread does not exist, or if `agent` or
/// `thread_id` is empty.
pub fn join_thread(settings: &Settings, request: &ThreadMemberRequest<'_>) -> Result<Thread> {
    let thread_id = non_empty(request.thread_id, "thread_id")?;
    let agent = non_empty(request.agent, "agent")?;

    let mut conn = redis_bus::connect(settings)?;
    redis_bus::join_thread(&mut conn, thread_id, agent)
}

/// Remove an agent from a thread.
///
/// # Errors
///
/// Returns an error if the thread does not exist, or if `agent` or
/// `thread_id` is empty.
pub fn leave_thread(settings: &Settings, request: &ThreadMemberRequest<'_>) -> Result<Thread> {
    let thread_id = non_empty(request.thread_id, "thread_id")?;
    let agent = non_empty(request.agent, "agent")?;

    let mut conn = redis_bus::connect(settings)?;
    redis_bus::leave_thread(&mut conn, thread_id, agent)
}

/// Get a single thread by ID.
///
/// # Errors
///
/// Returns an error if `thread_id` is empty or if the Redis connection fails.
pub fn get_thread(settings: &Settings, thread_id: &str) -> Result<Option<Thread>> {
    let id = non_empty(thread_id, "thread_id")?;
    let mut conn = redis_bus::connect(settings)?;
    redis_bus::get_thread(&mut conn, id)
}

/// List all threads.
///
/// # Errors
///
/// Returns an error if the Redis connection fails.
pub fn list_threads(settings: &Settings) -> Result<Vec<Thread>> {
    let mut conn = redis_bus::connect(settings)?;
    redis_bus::list_threads(&mut conn)
}

/// Close a thread (set status to Closed).
///
/// # Errors
///
/// Returns an error if the thread does not exist or `thread_id` is empty.
pub fn close_thread(settings: &Settings, thread_id: &str) -> Result<Thread> {
    let id = non_empty(thread_id, "thread_id")?;
    let mut conn = redis_bus::connect(settings)?;
    redis_bus::close_thread(&mut conn, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_settings() -> Settings {
        Settings::from_env()
    }

    #[test]
    fn create_thread_rejects_blank_created_by_before_connecting() {
        let err = create_thread(
            &test_settings(),
            &CreateThreadRequest {
                thread_id: Some("thread-1"),
                created_by: "   ",
                repo: Some("agent-bus"),
                topic: Some("review"),
            },
        )
        .expect_err("blank creator must fail");

        assert!(err.to_string().contains("created_by"));
    }

    #[test]
    fn join_thread_rejects_blank_thread_id_before_connecting() {
        let err = join_thread(
            &test_settings(),
            &ThreadMemberRequest {
                thread_id: "   ",
                agent: "codex",
            },
        )
        .expect_err("blank thread_id must fail");

        assert!(err.to_string().contains("thread_id"));
    }

    #[test]
    fn join_thread_rejects_blank_agent_before_connecting() {
        let err = join_thread(
            &test_settings(),
            &ThreadMemberRequest {
                thread_id: "thread-1",
                agent: "   ",
            },
        )
        .expect_err("blank agent must fail");

        assert!(err.to_string().contains("agent"));
    }

    #[test]
    fn leave_thread_rejects_blank_thread_id_before_connecting() {
        let err = leave_thread(
            &test_settings(),
            &ThreadMemberRequest {
                thread_id: "   ",
                agent: "codex",
            },
        )
        .expect_err("blank thread_id must fail");

        assert!(err.to_string().contains("thread_id"));
    }

    #[test]
    fn leave_thread_rejects_blank_agent_before_connecting() {
        let err = leave_thread(
            &test_settings(),
            &ThreadMemberRequest {
                thread_id: "thread-1",
                agent: "   ",
            },
        )
        .expect_err("blank agent must fail");

        assert!(err.to_string().contains("agent"));
    }

    #[test]
    fn get_thread_rejects_blank_thread_id_before_connecting() {
        let err =
            get_thread(&test_settings(), "   ").expect_err("blank thread_id must fail before IO");

        assert!(err.to_string().contains("thread_id"));
    }

    #[test]
    fn close_thread_rejects_blank_thread_id_before_connecting() {
        let err =
            close_thread(&test_settings(), "   ").expect_err("blank thread_id must fail before IO");

        assert!(err.to_string().contains("thread_id"));
    }
}
