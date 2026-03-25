//! Message posting, acking, presence, and list operations.

use anyhow::Result;

use crate::models::{Message, Presence};
use crate::redis_bus::{
    bus_list_messages_from_redis_with_filters, bus_list_messages_with_filters, bus_post_message,
    bus_set_presence, clear_pending_ack,
};
use crate::settings::Settings;

use super::{MessageFilters, scoped_required_tags};

#[derive(Debug)]
pub struct PostMessageRequest<'a> {
    pub sender: &'a str,
    pub recipient: &'a str,
    pub topic: &'a str,
    pub body: &'a str,
    pub thread_id: Option<&'a str>,
    pub tags: &'a [String],
    pub priority: &'a str,
    pub request_ack: bool,
    pub reply_to: Option<&'a str>,
    pub metadata: &'a serde_json::Value,
    pub has_sse_subscribers: bool,
}

#[derive(Debug)]
pub struct AckMessageRequest<'a> {
    pub agent: &'a str,
    pub message_id: &'a str,
    pub body: &'a str,
    pub has_sse_subscribers: bool,
}

#[derive(Debug)]
pub struct AckMessageResult {
    pub message: Message,
    pub acked_message_id: String,
}

#[derive(Debug)]
pub struct PresenceRequest<'a> {
    pub agent: &'a str,
    pub status: &'a str,
    pub session_id: Option<&'a str>,
    pub capabilities: &'a [String],
    pub ttl_seconds: u64,
    pub metadata: &'a serde_json::Value,
}

#[derive(Debug)]
pub struct ReadMessagesRequest<'a> {
    pub agent: Option<&'a str>,
    pub from_agent: Option<&'a str>,
    pub since_minutes: u64,
    pub limit: usize,
    pub include_broadcast: bool,
    pub filters: MessageFilters<'a>,
}

#[must_use] 
pub fn knock_metadata(request_ack: bool) -> serde_json::Value {
    serde_json::json!({
        "knock": true,
        "delivery_hint": "sse",
        "expected_response_kind": if request_ack { "ack" } else { "status" }
    })
}

/// Post a message to the bus stream.
///
/// # Errors
///
/// Returns an error if the Redis `XADD` command fails or if body compression
/// encounters an unexpected error.
pub fn post_message(
    conn: &mut redis::Connection,
    settings: &Settings,
    request: &PostMessageRequest<'_>,
) -> Result<Message> {
    bus_post_message(
        conn,
        settings,
        request.sender,
        request.recipient,
        request.topic,
        request.body,
        request.thread_id,
        request.tags,
        request.priority,
        request.request_ack,
        request.reply_to,
        request.metadata,
        crate::pg_writer(),
        request.has_sse_subscribers,
    )
}

/// Post an acknowledgement message for `request.message_id`.
///
/// # Errors
///
/// Returns an error if posting the ack message to Redis fails.
pub fn post_ack(
    conn: &mut redis::Connection,
    settings: &Settings,
    request: &AckMessageRequest<'_>,
) -> Result<AckMessageResult> {
    let metadata = serde_json::json!({"ack_for": request.message_id});
    let message = post_message(
        conn,
        settings,
        &PostMessageRequest {
            sender: request.agent,
            recipient: "all",
            topic: "ack",
            body: request.body,
            thread_id: None,
            tags: &[],
            priority: "normal",
            request_ack: false,
            reply_to: Some(request.message_id),
            metadata: &metadata,
            has_sse_subscribers: request.has_sse_subscribers,
        },
    )?;

    if let Err(error) = clear_pending_ack(conn, request.message_id) {
        tracing::warn!(
            "failed to clear pending ack for {}: {error:#}",
            request.message_id
        );
    }

    Ok(AckMessageResult {
        message,
        acked_message_id: request.message_id.to_owned(),
    })
}

/// Set an agent's presence record.
///
/// # Errors
///
/// Returns an error if the Redis `SET EX` or `PUBLISH` commands fail.
pub fn set_presence(
    conn: &mut redis::Connection,
    settings: &Settings,
    request: &PresenceRequest<'_>,
) -> Result<Presence> {
    bus_set_presence(
        conn,
        settings,
        request.agent,
        request.status,
        request.session_id,
        request.capabilities,
        request.ttl_seconds,
        request.metadata,
        crate::pg_writer(),
    )
}

/// List messages from durable history (`PostgreSQL` with Redis fallback).
///
/// # Errors
///
/// Returns an error if both the `PostgreSQL` query and the Redis fallback fail.
pub fn list_messages_history(
    settings: &Settings,
    request: &ReadMessagesRequest<'_>,
) -> Result<Vec<Message>> {
    let required_tags = scoped_required_tags(&request.filters);
    let required_tag_refs: Vec<&str> = required_tags.iter().map(String::as_str).collect();
    bus_list_messages_with_filters(
        settings,
        request.agent,
        request.from_agent,
        request.since_minutes,
        request.limit,
        request.include_broadcast,
        request.filters.thread_id,
        &required_tag_refs,
    )
}

/// List messages directly from the live Redis stream.
///
/// # Errors
///
/// Returns an error if the Redis `XREVRANGE` command fails.
pub fn list_messages_live(
    conn: &mut redis::Connection,
    settings: &Settings,
    request: &ReadMessagesRequest<'_>,
) -> Result<Vec<Message>> {
    let required_tags = scoped_required_tags(&request.filters);
    let required_tag_refs: Vec<&str> = required_tags.iter().map(String::as_str).collect();
    bus_list_messages_from_redis_with_filters(
        conn,
        settings,
        request.agent,
        request.from_agent,
        request.since_minutes,
        request.limit,
        request.include_broadcast,
        request.filters.thread_id,
        &required_tag_refs,
    )
}
