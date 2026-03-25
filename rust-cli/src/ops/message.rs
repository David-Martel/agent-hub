//! Message posting, acking, presence, and list operations.

use anyhow::Result;

use crate::models::{Message, Presence};
use crate::redis_bus::{
    bus_list_messages_from_redis_with_filters, bus_list_messages_with_filters, bus_post_message,
    bus_set_presence, clear_pending_ack,
};
use crate::settings::Settings;

use super::{MessageFilters, scoped_required_tags};

pub(crate) struct PostMessageRequest<'a> {
    pub(crate) sender: &'a str,
    pub(crate) recipient: &'a str,
    pub(crate) topic: &'a str,
    pub(crate) body: &'a str,
    pub(crate) thread_id: Option<&'a str>,
    pub(crate) tags: &'a [String],
    pub(crate) priority: &'a str,
    pub(crate) request_ack: bool,
    pub(crate) reply_to: Option<&'a str>,
    pub(crate) metadata: &'a serde_json::Value,
    pub(crate) has_sse_subscribers: bool,
}

pub(crate) struct AckMessageRequest<'a> {
    pub(crate) agent: &'a str,
    pub(crate) message_id: &'a str,
    pub(crate) body: &'a str,
    pub(crate) has_sse_subscribers: bool,
}

pub(crate) struct AckMessageResult {
    pub(crate) message: Message,
    pub(crate) acked_message_id: String,
}

pub(crate) struct PresenceRequest<'a> {
    pub(crate) agent: &'a str,
    pub(crate) status: &'a str,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) capabilities: &'a [String],
    pub(crate) ttl_seconds: u64,
    pub(crate) metadata: &'a serde_json::Value,
}

pub(crate) struct ReadMessagesRequest<'a> {
    pub(crate) agent: Option<&'a str>,
    pub(crate) from_agent: Option<&'a str>,
    pub(crate) since_minutes: u64,
    pub(crate) limit: usize,
    pub(crate) include_broadcast: bool,
    pub(crate) filters: MessageFilters<'a>,
}

pub(crate) fn knock_metadata(request_ack: bool) -> serde_json::Value {
    serde_json::json!({
        "knock": true,
        "delivery_hint": "sse",
        "expected_response_kind": if request_ack { "ack" } else { "status" }
    })
}

pub(crate) fn post_message(
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

pub(crate) fn post_ack(
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

pub(crate) fn set_presence(
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

pub(crate) fn list_messages_history(
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

pub(crate) fn list_messages_live(
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
