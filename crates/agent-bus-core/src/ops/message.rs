//! Message posting, acking, presence, and list operations.

use anyhow::Result;

use crate::models::{Message, Presence};
use crate::redis_bus::{
    BatchSendPayload, PostedMessage, bus_list_messages_from_redis_with_filters,
    bus_list_messages_with_filters, bus_post_message, bus_post_messages_batch_with_notifications,
    bus_set_presence, clear_pending_ack,
};
use crate::settings::Settings;
use crate::validation::{
    auto_fit_schema, enforce_schema_for_transport, non_empty, validate_message_schema,
    validate_priority,
};

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

/// Raw inputs for [`validated_post_message`], before validation and schema
/// enforcement.
///
/// All string fields are validated (trimmed, non-empty) inside the function.
/// The `transport` field controls schema inference: `"mcp"` and `"http"`
/// default to the `status` schema for unknown topics, while `"cli"` (and any
/// other value) leaves the schema optional.
#[derive(Debug)]
pub struct ValidatedSendRequest<'a> {
    pub sender: &'a str,
    pub recipient: &'a str,
    pub topic: &'a str,
    pub body: &'a str,
    pub priority: &'a str,
    pub schema: Option<&'a str>,
    pub tags: &'a [String],
    pub thread_id: Option<&'a str>,
    pub reply_to: Option<&'a str>,
    pub request_ack: bool,
    pub metadata: &'a serde_json::Value,
    pub transport: &'a str,
    pub has_sse_subscribers: bool,
}

/// Validate inputs, enforce/infer schema, auto-fit the body, then post the
/// message to the bus.
///
/// This consolidates the send-message orchestration that was previously
/// duplicated across the CLI, HTTP, and MCP transports.
///
/// # Errors
///
/// Returns an error if any validation step fails (priority, non-empty fields,
/// schema) or if the underlying [`post_message`] call fails.
pub fn validated_post_message(
    conn: &mut redis::Connection,
    settings: &Settings,
    req: &ValidatedSendRequest<'_>,
) -> Result<Message> {
    validate_priority(req.priority)?;
    let sender = non_empty(req.sender, "sender")?;
    let recipient = non_empty(req.recipient, "recipient")?;
    let topic = non_empty(req.topic, "topic")?;
    let body = non_empty(req.body, "body")?;

    let effective_schema = enforce_schema_for_transport(req.transport, req.schema, topic);
    let fitted_body = auto_fit_schema(body, effective_schema);
    validate_message_schema(&fitted_body, effective_schema)?;

    post_message(
        conn,
        settings,
        &PostMessageRequest {
            sender,
            recipient,
            topic,
            body: &fitted_body,
            thread_id: req.thread_id,
            tags: req.tags,
            priority: req.priority,
            request_ack: req.request_ack,
            reply_to: req.reply_to,
            metadata: req.metadata,
            has_sse_subscribers: req.has_sse_subscribers,
        },
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

// ---------------------------------------------------------------------------
// Batch send
// ---------------------------------------------------------------------------

/// A single item in a validated batch send request.
///
/// Similar to [`ValidatedSendRequest`] but without the `has_sse_subscribers`
/// and `transport` fields, which are batch-level concerns passed separately
/// to [`validated_batch_send`].
#[derive(Debug)]
pub struct ValidatedBatchItem {
    pub sender: String,
    pub recipient: String,
    pub topic: String,
    pub body: String,
    pub priority: String,
    pub schema: Option<String>,
    pub tags: Vec<String>,
    pub thread_id: Option<String>,
    pub reply_to: Option<String>,
    pub request_ack: bool,
    pub metadata: serde_json::Value,
}

/// Validate a batch of messages and post them in a single Redis pipeline
/// round-trip.
///
/// Each item is validated (priority, non-empty fields, schema
/// enforcement/fitting) and the first failing item causes the entire batch to
/// be rejected.  On success the messages are posted atomically via
/// [`bus_post_messages_batch_with_notifications`].
///
/// # Errors
///
/// Returns an error naming the first invalid item (by index) or if the
/// underlying Redis pipeline write fails.
pub fn validated_batch_send(
    conn: &mut redis::Connection,
    settings: &Settings,
    items: &[ValidatedBatchItem],
    transport: &str,
    has_sse_subscribers: bool,
) -> Result<Vec<PostedMessage>> {
    use anyhow::Context as _;

    let mut payloads: Vec<BatchSendPayload> = Vec::with_capacity(items.len());

    for (idx, item) in items.iter().enumerate() {
        let ctx = || format!("item {idx}");

        validate_priority(&item.priority).with_context(ctx)?;
        let sender = non_empty(&item.sender, "sender").with_context(ctx)?;
        let recipient = non_empty(&item.recipient, "recipient").with_context(ctx)?;
        let topic = non_empty(&item.topic, "topic").with_context(ctx)?;
        let body = non_empty(&item.body, "body").with_context(ctx)?;

        let effective_schema =
            enforce_schema_for_transport(transport, item.schema.as_deref(), topic);
        let fitted_body = auto_fit_schema(body, effective_schema);
        validate_message_schema(&fitted_body, effective_schema)
            .with_context(|| format!("item {idx}: schema validation failed"))?;

        let metadata = if item.metadata.is_null() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            item.metadata.clone()
        };

        payloads.push(BatchSendPayload {
            from: sender.to_owned(),
            to: recipient.to_owned(),
            topic: topic.to_owned(),
            body: fitted_body,
            thread_id: item.thread_id.clone(),
            tags: item.tags.clone(),
            priority: item.priority.clone(),
            request_ack: item.request_ack,
            reply_to: item.reply_to.clone(),
            metadata,
        });
    }

    bus_post_messages_batch_with_notifications(
        conn,
        settings,
        payloads,
        crate::pg_writer(),
        has_sse_subscribers,
    )
}
