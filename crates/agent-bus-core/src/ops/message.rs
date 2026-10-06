//! Message posting, acking, presence, and list operations.

use crate::error::Result;

use crate::models::{Message, Presence};
use crate::redis_bus::{
    BatchSendPayload, CheckedBatchSendPayload, PostedMessage,
    bus_list_messages_from_redis_with_filters, bus_list_messages_with_filters, bus_post_message,
    bus_post_message_with_schema, bus_post_messages_batch_with_schema, bus_set_presence,
    clear_pending_ack,
};
use crate::settings::Settings;
use crate::validation::{
    auto_fit_schema, non_empty, resolve_message_schema, validate_message_schema, validate_priority,
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

    let schema = resolve_message_schema(req.transport, req.schema, topic)?;
    let effective_schema = schema.map(crate::validation::MessageSchema::as_str);
    let fitted_body = auto_fit_schema(body, effective_schema);
    validate_message_schema(&fitted_body, effective_schema)?;

    Ok(bus_post_message_with_schema(
        conn,
        settings,
        sender,
        recipient,
        topic,
        &fitted_body,
        req.thread_id,
        req.tags,
        req.priority,
        req.request_ack,
        req.reply_to,
        req.metadata,
        crate::pg_writer(),
        req.has_sse_subscribers,
        schema,
    )?
    .message)
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

    // Also clear the ack deadline record if one exists (non-fatal).
    if let Err(error) = crate::ops::ack_deadline::clear_deadline(conn, request.message_id) {
        tracing::warn!(
            "failed to clear ack deadline for {}: {error:#}",
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
    let agent = non_empty(request.agent, "agent")?;

    bus_set_presence(
        conn,
        settings,
        agent,
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
        request.filters.topic,
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
        request.filters.topic,
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
/// [`bus_post_messages_batch_with_schema`].
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
    let mut payloads: Vec<CheckedBatchSendPayload> = Vec::with_capacity(items.len());

    for (idx, item) in items.iter().enumerate() {
        // Tag every per-item validation failure with its batch index *and* the
        // underlying cause, so a rejected batch names which item failed and why
        // (errors must be captured, never silently collapsed to "batch failed").
        let at = |e: crate::error::AgentBusError| {
            crate::error::AgentBusError::InvalidParams(format!("item {idx}: {e}"))
        };

        validate_priority(&item.priority).map_err(at)?;
        let sender = non_empty(&item.sender, "sender").map_err(at)?;
        let recipient = non_empty(&item.recipient, "recipient").map_err(at)?;
        let topic = non_empty(&item.topic, "topic").map_err(at)?;
        let body = non_empty(&item.body, "body").map_err(at)?;

        let schema =
            resolve_message_schema(transport, item.schema.as_deref(), topic).map_err(at)?;
        let effective_schema = schema.map(crate::validation::MessageSchema::as_str);
        let fitted_body = auto_fit_schema(body, effective_schema);
        validate_message_schema(&fitted_body, effective_schema).map_err(|e| {
            crate::error::AgentBusError::Internal(format!(
                "item {idx}: schema validation failed: {e}"
            ))
        })?;

        let metadata = if item.metadata.is_null() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            item.metadata.clone()
        };

        payloads.push(CheckedBatchSendPayload {
            schema,
            payload: BatchSendPayload {
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
            },
        });
    }

    bus_post_messages_batch_with_schema(
        conn,
        settings,
        payloads,
        crate::pg_writer(),
        has_sse_subscribers,
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// RESP fixture captures real production XADD fields on an ephemeral socket.
    /// It has no backing Redis or PostgreSQL service and accepts one connection.
    struct RedisFixture {
        url: String,
        commands: std::sync::Arc<std::sync::Mutex<Vec<Vec<String>>>>,
        worker: Option<std::thread::JoinHandle<()>>,
    }

    impl RedisFixture {
        fn new() -> Self {
            use std::io::{BufRead, Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("redis://{}/0", listener.local_addr().unwrap());
            let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let captured = std::sync::Arc::clone(&commands);
            let worker = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut reader = std::io::BufReader::new(socket.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 {
                        break;
                    }
                    assert!(line.starts_with('*'));
                    let count: usize = line[1..].trim().parse().unwrap();
                    let mut args = Vec::new();
                    for _ in 0..count {
                        line.clear();
                        reader.read_line(&mut line).unwrap();
                        assert!(line.starts_with('$'));
                        let size: usize = line[1..].trim().parse().unwrap();
                        let mut bytes = vec![0; size + 2];
                        reader.read_exact(&mut bytes).unwrap();
                        assert_eq!(&bytes[size..], b"\r\n");
                        bytes.truncate(size);
                        args.push(String::from_utf8(bytes).unwrap());
                    }
                    let response = match args[0].as_str() {
                        "XADD" => "$3\r\n1-0\r\n",
                        "CLIENT" => "+OK\r\n",
                        _ => ":0\r\n",
                    };
                    captured.lock().unwrap().push(args);
                    socket.write_all(response.as_bytes()).unwrap();
                }
            });
            Self {
                url,
                commands,
                worker: Some(worker),
            }
        }

        fn connection(&self) -> redis::Connection {
            redis::Client::open(self.url.as_str())
                .unwrap()
                .get_connection()
                .unwrap()
        }

        fn xadds(&self) -> Vec<Vec<String>> {
            self.commands
                .lock()
                .unwrap()
                .iter()
                .filter(|cmd| cmd[0] == "XADD")
                .cloned()
                .collect()
        }
    }

    impl Drop for RedisFixture {
        fn drop(&mut self) {
            self.worker.take().unwrap().join().unwrap();
        }
    }

    fn stored_field<'a>(command: &'a [String], name: &str) -> &'a str {
        command[6..]
            .as_chunks::<2>()
            .0
            .iter()
            .find(|pair| pair[0] == name)
            .unwrap()[1]
            .as_str()
    }

    #[test]
    fn explicit_schema_survives_validated_send_all_transports_and_compression() {
        for transport in ["cli", "http", "mcp"] {
            for large in [false, true] {
                let fixture = RedisFixture::new();
                let mut conn = fixture.connection();
                let settings = crate::test_support::offline_settings();
                let body = if large {
                    "CLEAR ".repeat(2_000)
                } else {
                    "CLEAR".to_owned()
                };
                let metadata = serde_json::json!({"_schema":"benchmark", "receipt":"kept"});
                let message = validated_post_message(
                    &mut conn,
                    &settings,
                    &ValidatedSendRequest {
                        sender: "test",
                        recipient: "all",
                        topic: "review",
                        body: &body,
                        priority: "normal",
                        schema: Some("status"),
                        tags: &[],
                        thread_id: None,
                        reply_to: None,
                        request_ack: false,
                        metadata: &metadata,
                        transport,
                        has_sse_subscribers: false,
                    },
                )
                .unwrap();
                assert_eq!(message.body, body.trim());
                assert_eq!(message.metadata["_schema"], "status");
                assert_eq!(message.metadata["receipt"], "kept");
                let records = fixture.xadds();
                assert_eq!(records.len(), 1);
                let stored: serde_json::Value =
                    serde_json::from_str(stored_field(&records[0], "metadata")).unwrap();
                assert_eq!(stored["_schema"], "status");
                let stored_body = stored_field(&records[0], "body");
                if large {
                    assert_eq!(stored["_compressed"], "lz4");
                    assert_eq!(
                        {
                            use base64::Engine as _;
                            let bytes = base64::engine::general_purpose::STANDARD
                                .decode(stored_body)
                                .unwrap();
                            String::from_utf8(lz4_flex::decompress_size_prepended(&bytes).unwrap())
                                .unwrap()
                        },
                        body.trim()
                    );
                } else {
                    assert_eq!(stored_body, body);
                }
                // PgWriter receives this exact Message; serialization must preserve authority.
                let pg_record = serde_json::to_value(&message).unwrap();
                assert_eq!(pg_record["metadata"]["_schema"], stored["_schema"]);
                drop(conn);
            }
        }
    }

    #[test]
    fn batch_explicit_inferred_and_unknown_topic_annotations_are_authoritative() {
        let fixture = RedisFixture::new();
        let mut conn = fixture.connection();
        let settings = crate::test_support::offline_settings();
        let items: Vec<_> = [
            ("review", Some("status")),
            ("review", None),
            ("custom", None),
        ]
        .into_iter()
        .map(|(topic, schema)| ValidatedBatchItem {
            sender: "test".to_owned(),
            recipient: "all".to_owned(),
            topic: topic.to_owned(),
            body: "CLEAR ".repeat(2_000),
            priority: "normal".to_owned(),
            schema: schema.map(str::to_owned),
            tags: vec![],
            thread_id: None,
            reply_to: None,
            request_ack: false,
            metadata: serde_json::json!({"_schema":"benchmark"}),
        })
        .collect();
        let messages = validated_batch_send(&mut conn, &settings, &items, "cli", false).unwrap();
        let records = fixture.xadds();
        assert_eq!(records.len(), 3);
        for (index, expected) in [Some("status"), Some("finding"), None]
            .into_iter()
            .enumerate()
        {
            let stored: serde_json::Value =
                serde_json::from_str(stored_field(&records[index], "metadata")).unwrap();
            assert_eq!(
                stored.get("_schema").and_then(serde_json::Value::as_str),
                expected
            );
            assert_eq!(
                messages[index]
                    .message
                    .metadata
                    .get("_schema")
                    .and_then(serde_json::Value::as_str),
                expected
            );
            assert_eq!(stored["_compressed"], "lz4");
        }
        drop(conn);
    }

    #[test]
    fn unknown_explicit_schema_rejected_without_any_storage_write() {
        let fixture = RedisFixture::new();
        let mut conn = fixture.connection();
        let settings = crate::test_support::offline_settings();
        for transport in ["cli", "http", "mcp"] {
            let result = validated_post_message(
                &mut conn,
                &settings,
                &ValidatedSendRequest {
                    sender: "test",
                    recipient: "all",
                    topic: "review",
                    body: "CLEAR",
                    priority: "normal",
                    schema: Some("bogus"),
                    tags: &[],
                    thread_id: None,
                    reply_to: None,
                    request_ack: false,
                    metadata: &serde_json::json!({}),
                    transport,
                    has_sse_subscribers: false,
                },
            );
            assert!(result.unwrap_err().to_string().contains("unknown schema"));
        }
        assert!(fixture.xadds().is_empty());
        drop(conn);
    }

    #[test]
    fn omitted_schema_keeps_transport_defaults_and_removes_spoofed_annotations() {
        for (transport, topic, expected) in [
            ("cli", "review", Some("finding")),
            ("cli", "custom", None),
            ("http", "custom", Some("status")),
            ("mcp", "custom", Some("status")),
        ] {
            let fixture = RedisFixture::new();
            let mut conn = fixture.connection();
            let settings = crate::test_support::offline_settings();
            let message = validated_post_message(
                &mut conn,
                &settings,
                &ValidatedSendRequest {
                    sender: "test",
                    recipient: "all",
                    topic,
                    body: "CLEAR",
                    priority: "normal",
                    schema: None,
                    tags: &[],
                    thread_id: None,
                    reply_to: None,
                    request_ack: false,
                    metadata: &serde_json::json!({"_schema":"benchmark"}),
                    transport,
                    has_sse_subscribers: false,
                },
            )
            .unwrap();
            let records = fixture.xadds();
            let stored: serde_json::Value =
                serde_json::from_str(stored_field(&records[0], "metadata")).unwrap();
            assert_eq!(
                stored.get("_schema").and_then(serde_json::Value::as_str),
                expected
            );
            assert_eq!(
                message
                    .metadata
                    .get("_schema")
                    .and_then(serde_json::Value::as_str),
                expected
            );
            drop(conn);
        }
    }

    #[test]
    fn invalid_later_batch_schema_rejects_entire_batch_before_storage() {
        let fixture = RedisFixture::new();
        let mut conn = fixture.connection();
        let settings = crate::test_support::offline_settings();
        let items: Vec<_> = ["status", "bogus"]
            .into_iter()
            .map(|schema| ValidatedBatchItem {
                sender: "test".to_owned(),
                recipient: "all".to_owned(),
                topic: "review".to_owned(),
                body: "CLEAR".to_owned(),
                priority: "normal".to_owned(),
                schema: Some(schema.to_owned()),
                tags: vec![],
                thread_id: None,
                reply_to: None,
                request_ack: false,
                metadata: serde_json::json!({}),
            })
            .collect();
        for transport in ["cli", "http", "mcp"] {
            let error =
                validated_batch_send(&mut conn, &settings, &items, transport, false).unwrap_err();
            assert!(error.to_string().contains("item 1"));
            assert!(error.to_string().contains("unknown schema"));
        }
        assert!(fixture.xadds().is_empty());
        drop(conn);
    }

    #[test]
    fn actual_mcp_dispatch_preserves_explicit_schema_and_rejects_unknown() {
        let fixture = RedisFixture::new();
        let mut settings = crate::test_support::offline_settings();
        settings.redis_url = fixture.url.clone();
        let dispatcher = crate::mcp_dispatch::McpToolDispatch::new(&settings);
        let mut args = serde_json::json!({"sender":"test", "recipient":"all", "topic":"review",
            "body":"CLEAR", "schema":"status", "metadata":{"_schema":"finding"}})
        .as_object()
        .unwrap()
        .clone();
        let message = dispatcher.dispatch_tool("post_message", &args).unwrap();
        assert_eq!(message["metadata"]["_schema"], "status");
        assert_eq!(message["body"], "CLEAR");
        args.insert("schema".to_owned(), serde_json::json!("bogus"));
        assert!(dispatcher.dispatch_tool("post_message", &args).is_err());
        assert_eq!(fixture.xadds().len(), 1);
    }

    // -- validated_post_message: validation-only paths -------------------------
    //
    // `validated_post_message` requires a `redis::Connection` we cannot create
    // in unit tests.  However its validation calls (`validate_priority`,
    // `non_empty`, `enforce_schema_for_transport`, `validate_message_schema`)
    // all run BEFORE the Redis call.  We exercise the same validation pipeline
    // directly to prove the contract is correct.

    #[test]
    fn validated_send_rejects_empty_sender() {
        // The first non_empty inside validated_post_message is for sender.
        let result = non_empty("", "sender");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("sender"),
            "error should mention 'sender': {msg}"
        );
    }

    #[test]
    fn validated_send_rejects_whitespace_sender() {
        let result = non_empty("   ", "sender");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("sender"),
            "error should mention 'sender': {msg}"
        );
    }

    #[test]
    fn validated_send_rejects_empty_body() {
        let result = non_empty("", "body");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("body"), "error should mention 'body': {msg}");
    }

    #[test]
    fn validated_send_rejects_bad_priority() {
        let result = validate_priority("bogus");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("invalid priority"),
            "error should mention 'invalid priority': {msg}"
        );
    }

    #[test]
    fn legacy_schema_helper_unknown_name_retains_fallback_behavior() {
        // When transport="mcp" and schema=Some("bogus"), enforce_schema_for_transport
        // falls through the unknown explicit schema, finds no topic match for
        // "general", and defaults to "status".  So "bogus" does NOT propagate
        // as the effective schema — it is silently replaced.
        //
        // However, if the body does not satisfy the inferred schema, validation
        // will still fail.  We verify the full pipeline here.
        use crate::validation::{
            auto_fit_schema, enforce_schema_for_transport, validate_message_schema,
        };
        let effective = enforce_schema_for_transport("mcp", Some("bogus"), "general");
        // MCP with unknown explicit + unknown topic defaults to "status".
        assert_eq!(effective, Some("status"));
        let fitted = auto_fit_schema("non-empty body", effective);
        // "status" schema only requires non-empty, so this passes.
        assert!(validate_message_schema(&fitted, effective).is_ok());

        // An empty body under MCP's inferred status schema WILL fail.
        let fitted_empty = auto_fit_schema("", effective);
        assert!(validate_message_schema(&fitted_empty, effective).is_err());
    }

    // -- validated_batch_send: validation-only paths ---------------------------
    //
    // Same approach: verify the validation logic that runs before the Redis
    // pipeline call.

    #[test]
    fn batch_send_rejects_empty_sender_in_first_item() {
        // validated_batch_send iterates items and calls non_empty on sender.
        let result = non_empty("", "sender");
        assert!(result.is_err());
    }

    #[test]
    fn batch_send_rejects_empty_topic_in_item() {
        let result = non_empty("", "topic");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("topic"), "error should mention 'topic': {msg}");
    }

    #[test]
    fn batch_send_rejects_bad_priority_in_item() {
        let result = validate_priority("critical");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("invalid priority"),
            "error should mention 'invalid priority': {msg}"
        );
    }

    #[test]
    fn batch_item_schema_fitting_produces_valid_finding() {
        use crate::validation::{
            auto_fit_schema, enforce_schema_for_transport, validate_message_schema,
        };
        // Simulate what validated_batch_send does per item.
        let transport = "mcp";
        let topic = "code-findings";
        let body = "memory leak in allocator";
        let schema = None;

        let effective = enforce_schema_for_transport(transport, schema, topic);
        assert_eq!(effective, Some("finding"));
        let fitted = auto_fit_schema(body, effective);
        assert!(fitted.contains("FINDING:"));
        assert!(fitted.contains("SEVERITY:"));
        assert!(validate_message_schema(&fitted, effective).is_ok());
    }

    // -- set_presence validation ----------------------------------------------

    #[test]
    fn set_presence_rejects_empty_agent() {
        // set_presence calls non_empty(request.agent, "agent") first.
        let result = non_empty("", "agent");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("agent"), "error should mention 'agent': {msg}");
    }

    #[test]
    fn set_presence_rejects_whitespace_agent() {
        let result = non_empty("  \t  ", "agent");
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("agent"), "error should mention 'agent': {msg}");
    }

    // -- ValidatedBatchItem construction / field access ------------------------

    #[test]
    fn validated_batch_item_stores_all_fields() {
        let item = ValidatedBatchItem {
            sender: "claude".to_owned(),
            recipient: "codex".to_owned(),
            topic: "status".to_owned(),
            body: "ready to coordinate".to_owned(),
            priority: "normal".to_owned(),
            schema: Some("status".to_owned()),
            tags: vec!["repo:agent-bus".to_owned()],
            thread_id: Some("thread-1".to_owned()),
            reply_to: None,
            request_ack: true,
            metadata: serde_json::json!({"key": "value"}),
        };
        assert_eq!(item.sender, "claude");
        assert_eq!(item.recipient, "codex");
        assert_eq!(item.topic, "status");
        assert!(item.request_ack);
        assert_eq!(item.tags.len(), 1);
        assert_eq!(item.thread_id.as_deref(), Some("thread-1"));
        assert!(item.reply_to.is_none());
    }

    // -- knock_metadata -------------------------------------------------------

    #[test]
    fn knock_metadata_with_ack_sets_expected_response_kind() {
        let meta = knock_metadata(true);
        assert_eq!(meta["knock"], true);
        assert_eq!(meta["delivery_hint"], "sse");
        assert_eq!(meta["expected_response_kind"], "ack");
    }

    #[test]
    fn knock_metadata_without_ack_sets_status_response() {
        let meta = knock_metadata(false);
        assert_eq!(meta["knock"], true);
        assert_eq!(meta["expected_response_kind"], "status");
    }
}
