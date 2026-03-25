//! Typed channel operations wrapping [`crate::channels`].
//!
//! This module provides request structs and thin delegating functions so that
//! transport layers (CLI, HTTP, MCP) share a single typed interface rather
//! than calling [`crate::channels`] directly.
//!
//! All functions are pure wrappers — no business logic lives here.

use anyhow::Result;

use crate::models::Message;
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Direct channel
// ---------------------------------------------------------------------------

/// Typed request for posting a direct message between two agents.
pub(crate) struct PostDirectRequest<'a> {
    pub(crate) from_agent: &'a str,
    pub(crate) to_agent: &'a str,
    pub(crate) topic: &'a str,
    pub(crate) body: &'a str,
    pub(crate) thread_id: Option<&'a str>,
    pub(crate) tags: &'a [String],
}

/// Post a direct message and return the stored [`Message`].
///
/// Delegates to [`crate::channels::post_direct`].
///
/// # Errors
///
/// Returns an error if either agent ID or the body is empty, or if the Redis
/// `XADD` fails.
pub(crate) fn post_direct(
    settings: &Settings,
    request: &PostDirectRequest<'_>,
) -> Result<Message> {
    crate::channels::post_direct(
        settings,
        request.from_agent,
        request.to_agent,
        request.topic,
        request.body,
        request.thread_id,
        request.tags,
    )
}

/// Typed request for reading direct messages between two agents.
pub(crate) struct ReadDirectRequest<'a> {
    pub(crate) agent_a: &'a str,
    pub(crate) agent_b: &'a str,
    pub(crate) limit: usize,
}

/// Read up to `request.limit` messages from the direct channel.
///
/// Delegates to [`crate::channels::read_direct`].
///
/// # Errors
///
/// Returns an error if the Redis `XREVRANGE` fails.
pub(crate) fn read_direct(
    settings: &Settings,
    request: &ReadDirectRequest<'_>,
) -> Result<Vec<Message>> {
    crate::channels::read_direct(settings, request.agent_a, request.agent_b, request.limit)
}

// ---------------------------------------------------------------------------
// Group channel
// ---------------------------------------------------------------------------

/// Typed request for creating a named group channel.
pub(crate) struct CreateGroupRequest<'a> {
    pub(crate) name: &'a str,
    pub(crate) members: &'a [String],
    pub(crate) created_by: &'a str,
}

/// Create (or extend) a named group channel.
///
/// Delegates to [`crate::channels::create_group`].
///
/// # Errors
///
/// Returns an error if the group name is invalid or Redis commands fail.
pub(crate) fn create_group(
    settings: &Settings,
    request: &CreateGroupRequest<'_>,
) -> Result<crate::channels::GroupInfo> {
    crate::channels::create_group(
        settings,
        request.name,
        request.members,
        request.created_by,
    )
}

/// Typed request for posting a message to a named group channel.
pub(crate) struct PostGroupRequest<'a> {
    pub(crate) group: &'a str,
    pub(crate) from_agent: &'a str,
    pub(crate) topic: &'a str,
    pub(crate) body: &'a str,
    pub(crate) thread_id: Option<&'a str>,
}

/// Post a message to a named group channel.
///
/// Delegates to [`crate::channels::post_to_group`].
///
/// # Errors
///
/// Returns an error if the group does not exist or the Redis `XADD` fails.
pub(crate) fn post_group(
    settings: &Settings,
    request: &PostGroupRequest<'_>,
) -> Result<Message> {
    crate::channels::post_to_group(
        settings,
        request.group,
        request.from_agent,
        request.topic,
        request.body,
        request.thread_id,
    )
}

/// Typed request for reading messages from a named group channel.
pub(crate) struct ReadGroupRequest<'a> {
    pub(crate) group: &'a str,
    pub(crate) limit: usize,
}

/// Read up to `request.limit` messages from a named group channel.
///
/// Delegates to [`crate::channels::read_group`].
///
/// # Errors
///
/// Returns an error if the Redis `XREVRANGE` fails.
pub(crate) fn read_group(
    settings: &Settings,
    request: &ReadGroupRequest<'_>,
) -> Result<Vec<Message>> {
    crate::channels::read_group(settings, request.group, request.limit)
}

/// List all group channels that currently exist.
///
/// Delegates to [`crate::channels::list_groups`].
///
/// # Errors
///
/// Returns an error if the Redis `SCAN` fails.
pub(crate) fn list_groups(settings: &Settings) -> Result<Vec<crate::channels::GroupInfo>> {
    crate::channels::list_groups(settings)
}

// ---------------------------------------------------------------------------
// Escalation channel
// ---------------------------------------------------------------------------

/// Typed request for posting an escalation message.
pub(crate) struct EscalateRequest<'a> {
    pub(crate) from_agent: &'a str,
    pub(crate) body: &'a str,
    pub(crate) thread_id: Option<&'a str>,
    pub(crate) tags: &'a [String],
}

/// Post an escalation message to the first available orchestrator.
///
/// Delegates to [`crate::channels::post_escalation`].
///
/// # Errors
///
/// Returns an error if the body is empty or the Redis `XADD` fails.
pub(crate) fn post_escalation(
    settings: &Settings,
    request: &EscalateRequest<'_>,
) -> Result<Message> {
    crate::channels::post_escalation(
        settings,
        request.from_agent,
        request.body,
        request.thread_id,
        request.tags,
    )
}

// ---------------------------------------------------------------------------
// Channel summary
// ---------------------------------------------------------------------------

/// Return a summary of all active channels for display in the monitor.
///
/// Delegates to [`crate::channels::channel_summary`].
///
/// # Errors
///
/// Returns an error if Redis `SCAN` commands fail.
pub(crate) fn channel_summary(
    settings: &Settings,
) -> Result<crate::channels::ChannelSummary> {
    crate::channels::channel_summary(settings)
}
