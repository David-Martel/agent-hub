//! Transport-agnostic MCP tool dispatch.
//!
//! This module contains the tool definitions (schemas) and the dispatch logic
//! that routes a tool call by name to the appropriate core operation.  It uses
//! only [`serde_json::Value`] for schemas and arguments, so it does **not**
//! depend on `rmcp` or any other transport crate.
//!
//! Transport adapters (stdio via `rmcp`, Streamable HTTP, etc.) convert their
//! native request types to a `(name, args)` pair and delegate here.

use crate::error::Result;
use serde_json::Value;

/// Number of MCP tools exposed by [`tool_definitions`].
pub const TOOL_COUNT: usize = 17;

use crate::ops::admin::{health as ops_health, list_presence as ops_list_presence};
use crate::ops::channel::{
    CreateGroupRequest, EscalateRequest, PostDirectRequest, PostGroupRequest, ReadDirectRequest,
    ReadGroupRequest, create_group as ops_create_group, post_direct as ops_post_direct,
    post_escalation as ops_post_escalation, post_group as ops_post_group,
    read_direct as ops_read_direct, read_group as ops_read_group,
};
use crate::ops::claim::{
    ClaimResourceRequest, ReleaseClaimRequest, RenewClaimRequest, ResolveClaimRequest,
    claim_resource as ops_claim_resource, parse_resource_scope, release_claim as ops_release_claim,
    renew_claim as ops_renew_claim, resolve_claim as ops_resolve_claim,
};
use crate::ops::inbox::{CheckInboxRequest, check_inbox};
use crate::ops::{
    AckMessageRequest, MessageFilters, PostMessageRequest, PresenceRequest, ReadMessagesRequest,
    ValidatedSendRequest, knock_metadata, list_messages_history, post_ack, post_message,
    set_presence, validated_post_message,
};
use crate::redis_bus::connect;
use crate::settings::Settings;

// ---------------------------------------------------------------------------
// Tool definition (transport-agnostic)
// ---------------------------------------------------------------------------

/// A transport-agnostic tool definition.
///
/// Unlike `rmcp::model::Tool`, this struct carries its schema as a plain
/// [`serde_json::Value`] so that it can be used from any transport layer
/// without pulling in the `rmcp` dependency.
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    /// Machine-readable tool name (e.g. `"bus_health"`).
    pub name: String,
    /// Human-readable description shown to LLMs.
    pub description: String,
    /// JSON Schema object describing the tool's input parameters.
    pub schema: Value,
}

// ---------------------------------------------------------------------------
// Schema helpers
// ---------------------------------------------------------------------------

/// Tool input schemas, separated from execution logic.
#[expect(
    clippy::must_use_candidate,
    reason = "schema builder functions are pure but callers always use the result"
)]
pub mod schemas {
    use serde_json::Value;

    pub fn schema_for(props: Value, required: &[&str]) -> Value {
        let mut schema = serde_json::Map::new();
        schema.insert("type".to_owned(), serde_json::json!("object"));
        schema.insert("properties".to_owned(), props);
        if !required.is_empty() {
            schema.insert(
                "required".to_owned(),
                Value::Array(required.iter().map(|s| serde_json::json!(s)).collect()),
            );
        }
        Value::Object(schema)
    }

    pub fn bus_health() -> Value {
        schema_for(serde_json::json!({}), &[])
    }

    pub fn post_message() -> Value {
        schema_for(
            serde_json::json!({
                "sender":    {"type": "string"},
                "recipient": {"type": "string"},
                "topic":     {"type": "string"},
                "body":      {"type": "string"},
                "tags":      {"type": "array", "items": {"type": "string"}},
                "thread_id": {"type": "string"},
                "priority":  {"type": "string", "enum": ["low","normal","high","urgent"]},
                "request_ack": {"type": "boolean"},
                "reply_to":  {"type": "string"},
                "metadata":  {"type": "object"},
                "schema":    {"type": "string", "enum": ["finding", "status", "benchmark"]}
            }),
            &["sender", "recipient", "topic", "body"],
        )
    }

    pub fn list_messages() -> Value {
        schema_for(
            serde_json::json!({
                "agent":          {"type": "string"},
                "sender":         {"type": "string"},
                "repo":           {"type": "string"},
                "session":        {"type": "string"},
                "tag":            {"type": "array", "items": {"type": "string"}},
                "thread_id":      {"type": "string"},
                "since_minutes":  {"type": "integer", "minimum": 1, "maximum": 10080},
                "limit":          {"type": "integer", "minimum": 1, "maximum": 500},
                "include_broadcast": {"type": "boolean"}
            }),
            &[],
        )
    }

    pub fn ack_message() -> Value {
        schema_for(
            serde_json::json!({
                "agent":      {"type": "string"},
                "message_id": {"type": "string"},
                "body":       {"type": "string"}
            }),
            &["agent", "message_id"],
        )
    }

    pub fn set_presence() -> Value {
        schema_for(
            serde_json::json!({
                "agent":        {"type": "string"},
                "status":       {"type": "string"},
                "session_id":   {"type": "string"},
                "capabilities": {"type": "array", "items": {"type": "string"}},
                "ttl_seconds":  {"type": "integer", "minimum": 1, "maximum": 86400},
                "metadata":     {"type": "object"}
            }),
            &["agent"],
        )
    }

    pub fn list_presence() -> Value {
        schema_for(serde_json::json!({}), &[])
    }

    pub fn list_presence_history() -> Value {
        schema_for(
            serde_json::json!({
                "agent":         {"type": "string"},
                "since_minutes": {"type": "integer", "minimum": 1, "maximum": 10080},
                "limit":         {"type": "integer", "minimum": 1, "maximum": 500}
            }),
            &[],
        )
    }

    pub fn negotiate() -> Value {
        schema_for(serde_json::json!({}), &[])
    }

    pub fn create_channel() -> Value {
        schema_for(
            serde_json::json!({
                "channel_type": {"type": "string", "enum": ["direct", "group", "escalate", "arbitrate"]},
                "name":        {"type": "string", "description": "Group name (for group channels)"},
                "members":     {"type": "array", "items": {"type": "string"}, "description": "Initial members (for group channels)"},
                "created_by":  {"type": "string", "description": "Creating agent ID"}
            }),
            &["channel_type", "created_by"],
        )
    }

    pub fn post_to_channel() -> Value {
        schema_for(
            serde_json::json!({
                "channel_type": {"type": "string", "enum": ["direct", "group", "escalate"]},
                "sender":       {"type": "string"},
                "recipient":    {"type": "string", "description": "Agent ID (for direct) or group name (for group)"},
                "topic":        {"type": "string"},
                "body":         {"type": "string"},
                "thread_id":    {"type": "string"},
                "tags":         {"type": "array", "items": {"type": "string"}}
            }),
            &["channel_type", "sender", "body"],
        )
    }

    pub fn read_channel() -> Value {
        schema_for(
            serde_json::json!({
                "channel_type": {"type": "string", "enum": ["direct", "group"]},
                "agent_a":      {"type": "string", "description": "First agent (for direct)"},
                "agent_b":      {"type": "string", "description": "Second agent (for direct)"},
                "group_name":   {"type": "string", "description": "Group name (for group)"},
                "limit":        {"type": "integer", "minimum": 1, "maximum": 500}
            }),
            &["channel_type"],
        )
    }

    pub fn claim_resource() -> Value {
        schema_for(
            serde_json::json!({
                "resource": {"type": "string", "description": "File/directory path to claim"},
                "agent":    {"type": "string", "description": "Claiming agent ID"},
                "reason":   {"type": "string", "description": "Why this agent needs first-edit"},
                "mode": {"type": "string", "enum": ["shared", "shared_namespaced", "exclusive"]},
                "namespace": {"type": "string"},
                "scope_kind": {"type": "string"},
                "scope_path": {"type": "string"},
                "repo_scopes": {"type": "array", "items": {"type": "string"}},
                "thread_id": {"type": "string"},
                "lease_ttl_seconds": {"type": "integer", "minimum": 1, "maximum": 86400},
                "scope": {"type": "string", "enum": ["repo", "machine"], "description": "Visibility scope: repo (default) or machine. Auto-detected from resource name if omitted."}
            }),
            &["resource", "agent"],
        )
    }

    pub fn renew_claim() -> Value {
        schema_for(
            serde_json::json!({
                "resource": {"type": "string"},
                "agent": {"type": "string"},
                "lease_ttl_seconds": {"type": "integer", "minimum": 1, "maximum": 86400}
            }),
            &["resource", "agent"],
        )
    }

    pub fn release_claim() -> Value {
        schema_for(
            serde_json::json!({
                "resource": {"type": "string"},
                "agent": {"type": "string"}
            }),
            &["resource", "agent"],
        )
    }

    pub fn resolve_claim() -> Value {
        schema_for(
            serde_json::json!({
                "resource":     {"type": "string"},
                "winner":       {"type": "string", "description": "Agent ID that wins first-edit"},
                "reason":       {"type": "string", "description": "Rationale for the decision"},
                "resolved_by":  {"type": "string", "description": "Decision-maker agent ID"}
            }),
            &["resource", "winner"],
        )
    }

    pub fn check_inbox() -> Value {
        schema_for(
            serde_json::json!({
                "agent": {
                    "type": "string",
                    "description": "Agent ID to check inbox for"
                },
                "repo": {
                    "type": "string",
                    "description": "Restrict to repo:<value> tagged messages"
                },
                "session": {
                    "type": "string",
                    "description": "Restrict to session:<value> tagged messages"
                },
                "tag": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Require all listed tags"
                },
                "thread_id": {
                    "type": "string",
                    "description": "Restrict to one thread"
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum messages to return (default 10, max 100)",
                    "minimum": 1,
                    "maximum": 100
                },
                "reset_cursor": {
                    "type": "boolean",
                    "description": "When true, reset the cursor to 0-0 before reading (re-delivers all messages)"
                }
            }),
            &["agent"],
        )
    }

    pub fn knock_agent() -> Value {
        schema_for(
            serde_json::json!({
                "sender": {"type": "string"},
                "recipient": {"type": "string"},
                "body": {"type": "string"},
                "thread_id": {"type": "string"},
                "tags": {"type": "array", "items": {"type": "string"}},
                "request_ack": {"type": "boolean"}
            }),
            &["sender", "recipient"],
        )
    }
}

// ---------------------------------------------------------------------------
// Argument extraction helpers
// ---------------------------------------------------------------------------

/// Extract a string arg from the args map.
#[must_use]
pub fn get_str<'a>(params: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    params.get(key)?.as_str()
}

/// Extract a string arg with a default.
#[must_use]
pub fn get_str_or<'a>(
    params: &'a serde_json::Map<String, Value>,
    key: &str,
    default: &'a str,
) -> &'a str {
    params.get(key).and_then(|v| v.as_str()).unwrap_or(default)
}

/// Extract a boolean arg with a default.
#[must_use]
pub fn get_bool_or(params: &serde_json::Map<String, Value>, key: &str, default: bool) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(default)
}

/// Extract a u64 arg with a default.
#[must_use]
pub fn get_u64_or(params: &serde_json::Map<String, Value>, key: &str, default: u64) -> u64 {
    params.get(key).and_then(Value::as_u64).unwrap_or(default)
}

/// Extract a usize arg with a default.
#[must_use]
pub fn get_usize_or(params: &serde_json::Map<String, Value>, key: &str, default: usize) -> usize {
    params
        .get(key)
        .and_then(Value::as_u64)
        .and_then(|v| usize::try_from(v).ok())
        .unwrap_or(default)
}

/// Extract a string array arg, returning an empty vec if absent.
#[must_use]
pub fn get_string_array(params: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// Extract a JSON object arg, returning an empty object if absent or not an object.
#[must_use]
pub fn get_object_or_empty(params: &serde_json::Map<String, Value>, key: &str) -> Value {
    params
        .get(key)
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or(Value::Object(serde_json::Map::new()))
}

// ---------------------------------------------------------------------------
// Tool definitions
// ---------------------------------------------------------------------------

/// Return the full list of MCP tool definitions.
///
/// The returned [`ToolDefinition`] values carry plain JSON schemas and can be
/// converted to transport-specific types (e.g. `rmcp::model::Tool`) by the
/// caller.
#[must_use]
pub fn tool_definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition {
            name: "bus_health".to_owned(),
            description: "Check the health of the agent-bus Redis backend.".to_owned(),
            schema: schemas::bus_health(),
        },
        ToolDefinition {
            name: "post_message".to_owned(),
            description: "Post a message to the agent coordination bus.".to_owned(),
            schema: schemas::post_message(),
        },
        ToolDefinition {
            name: "list_messages".to_owned(),
            description: "List recent messages from the bus, optionally filtered by recipient, sender, repo, session, tag, or thread_id.".to_owned(),
            schema: schemas::list_messages(),
        },
        ToolDefinition {
            name: "ack_message".to_owned(),
            description: "Acknowledge a message by posting an ack reply.".to_owned(),
            schema: schemas::ack_message(),
        },
        ToolDefinition {
            name: "set_presence".to_owned(),
            description: "Announce or update agent presence on the bus.".to_owned(),
            schema: schemas::set_presence(),
        },
        ToolDefinition {
            name: "list_presence".to_owned(),
            description: "List all active agent presence records.".to_owned(),
            schema: schemas::list_presence(),
        },
        ToolDefinition {
            name: "list_presence_history".to_owned(),
            description: "Query PostgreSQL for historical presence events within a time window.".to_owned(),
            schema: schemas::list_presence_history(),
        },
        ToolDefinition {
            name: "negotiate".to_owned(),
            description: "Return server capabilities: protocol version, supported features, \
                 transports, schemas, and encoding formats.".to_owned(),
            schema: schemas::negotiate(),
        },
        ToolDefinition {
            name: "create_channel".to_owned(),
            description: "Create a new group channel with an initial member list.".to_owned(),
            schema: schemas::create_channel(),
        },
        ToolDefinition {
            name: "post_to_channel".to_owned(),
            description: "Post a message to a direct, group, or escalation channel.".to_owned(),
            schema: schemas::post_to_channel(),
        },
        ToolDefinition {
            name: "read_channel".to_owned(),
            description: "Read messages from a direct or group channel.".to_owned(),
            schema: schemas::read_channel(),
        },
        ToolDefinition {
            name: "claim_resource".to_owned(),
            description: "Claim first-edit ownership of a resource. First claim is auto-granted; \
                 contested claims trigger orchestrator escalation.".to_owned(),
            schema: schemas::claim_resource(),
        },
        ToolDefinition {
            name: "renew_claim".to_owned(),
            description: "Renew an active resource claim/lease before it expires.".to_owned(),
            schema: schemas::renew_claim(),
        },
        ToolDefinition {
            name: "release_claim".to_owned(),
            description: "Release an active resource claim/lease held by an agent.".to_owned(),
            schema: schemas::release_claim(),
        },
        ToolDefinition {
            name: "resolve_claim".to_owned(),
            description: "Resolve a contested ownership claim by naming a winner and sending \
                 direct notifications to all claimants.".to_owned(),
            schema: schemas::resolve_claim(),
        },
        ToolDefinition {
            name: "knock_agent".to_owned(),
            description: "Send a durable direct knock notification to make another agent check the bus now.".to_owned(),
            schema: schemas::knock_agent(),
        },
        ToolDefinition {
            name: "check_inbox".to_owned(),
            description: "Return new messages for an agent since the last check (cursor-based). \
                 The cursor advances automatically so repeated calls yield only new messages. \
                 Use reset_cursor=true to re-read from the beginning of the stream.".to_owned(),
            schema: schemas::check_inbox(),
        },
    ]
}

// ---------------------------------------------------------------------------
// McpToolDispatch
// ---------------------------------------------------------------------------

/// Transport-agnostic MCP tool dispatcher.
///
/// Holds a reference to [`Settings`] and creates Redis connections on demand.
/// Each call to [`dispatch_tool`](McpToolDispatch::dispatch_tool) opens a fresh
/// connection (matching the original per-call pattern used by the stdio MCP
/// handler).
#[derive(Debug)]
pub struct McpToolDispatch<'a> {
    settings: &'a Settings,
}

impl<'a> McpToolDispatch<'a> {
    /// Create a new dispatcher bound to the given settings.
    #[must_use]
    pub fn new(settings: &'a Settings) -> Self {
        Self { settings }
    }

    /// Dispatch a tool call by name and arguments, returning the result as JSON.
    ///
    /// # Errors
    ///
    /// Returns an error if the tool name is unknown, a required argument is
    /// missing, or the underlying operation fails.
    #[expect(
        clippy::too_many_lines,
        reason = "match on 17 tool names, each short -- extracting further would reduce clarity"
    )]
    pub fn dispatch_tool(
        &self,
        name: &str,
        args: &serde_json::Map<String, Value>,
    ) -> Result<Value> {
        let settings = self.settings;

        match name {
            "bus_health" => {
                // MCP stdio transport has no r2d2 pool; pass None so the health
                // check falls back to a single fresh connection.
                let h = ops_health(settings, None);
                Ok(serde_json::to_value(&h)?)
            }

            "post_message" => {
                let sender =
                    get_str(args, "sender").ok_or_else(|| crate::error::AgentBusError::Internal(format!("sender is required")))?;
                let recipient =
                    get_str(args, "recipient").ok_or_else(|| crate::error::AgentBusError::Internal(format!("recipient is required")))?;
                let topic = get_str(args, "topic").ok_or_else(|| crate::error::AgentBusError::Internal(format!("topic is required")))?;
                let body = get_str(args, "body").ok_or_else(|| crate::error::AgentBusError::Internal(format!("body is required")))?;
                let tags = get_string_array(args, "tags");
                let thread_id = get_str(args, "thread_id");
                let priority = get_str_or(args, "priority", "normal");
                let request_ack = get_bool_or(args, "request_ack", false);
                let reply_to = get_str(args, "reply_to");
                let metadata = get_object_or_empty(args, "metadata");
                let schema = get_str(args, "schema");

                let mut conn = connect(settings)?;
                let msg = validated_post_message(
                    &mut conn,
                    settings,
                    &ValidatedSendRequest {
                        sender,
                        recipient,
                        topic,
                        body,
                        priority,
                        schema,
                        tags: &tags,
                        thread_id,
                        reply_to,
                        request_ack,
                        metadata: &metadata,
                        transport: "mcp",
                        has_sse_subscribers: false,
                    },
                )?;
                Ok(serde_json::to_value(&msg)?)
            }

            "list_messages" => {
                let agent = get_str(args, "agent");
                let sender = get_str(args, "sender");
                let repo = get_str(args, "repo");
                let session = get_str(args, "session");
                let tags = get_string_array(args, "tag");
                let thread_id = get_str(args, "thread_id");
                let since_minutes = get_u64_or(args, "since_minutes", 1440);
                let limit = get_usize_or(args, "limit", 50);
                let include_broadcast = get_bool_or(args, "include_broadcast", true);
                let filters = MessageFilters {
                    repo,
                    session,
                    tags: &tags,
                    thread_id,
                };
                let msgs = list_messages_history(
                    settings,
                    &ReadMessagesRequest {
                        agent,
                        from_agent: sender,
                        since_minutes,
                        limit,
                        include_broadcast,
                        filters,
                    },
                )?;
                Ok(serde_json::to_value(&msgs)?)
            }

            "ack_message" => {
                let agent = get_str(args, "agent").ok_or_else(|| crate::error::AgentBusError::Internal(format!("agent is required")))?;
                let message_id =
                    get_str(args, "message_id").ok_or_else(|| crate::error::AgentBusError::Internal(format!("message_id is required")))?;
                let body = get_str_or(args, "body", "ack");

                let mut conn = connect(settings)?;
                let ack = post_ack(
                    &mut conn,
                    settings,
                    &AckMessageRequest {
                        agent,
                        message_id,
                        body,
                        has_sse_subscribers: false,
                    },
                )?;
                let response = serde_json::json!({
                    "ack_sent": true,
                    "ack_message_id": ack.message.id,
                    "acked_message_id": ack.acked_message_id,
                    "timestamp": ack.message.timestamp_utc,
                });
                Ok(response)
            }

            "set_presence" => {
                let agent = get_str(args, "agent").ok_or_else(|| crate::error::AgentBusError::Internal(format!("agent is required")))?;
                let status = get_str_or(args, "status", "online");
                let session_id = get_str(args, "session_id");
                let capabilities = get_string_array(args, "capabilities");
                let ttl_seconds = get_u64_or(args, "ttl_seconds", 180);
                let metadata = get_object_or_empty(args, "metadata");

                let mut conn = connect(settings)?;
                let presence = set_presence(
                    &mut conn,
                    settings,
                    &PresenceRequest {
                        agent,
                        status,
                        session_id,
                        capabilities: &capabilities,
                        ttl_seconds,
                        metadata: &metadata,
                    },
                )?;
                Ok(serde_json::to_value(&presence)?)
            }

            "list_presence" => {
                let mut conn = connect(settings)?;
                let results = ops_list_presence(&mut conn, settings)?;
                Ok(serde_json::to_value(&results)?)
            }

            "list_presence_history" => {
                let agent = get_str(args, "agent");
                let since_minutes = get_u64_or(args, "since_minutes", 1440);
                let limit = get_usize_or(args, "limit", 50);

                let events = crate::postgres_store::list_presence_history_postgres(
                    settings,
                    agent,
                    since_minutes,
                    limit,
                )?;
                Ok(serde_json::to_value(&events)?)
            }

            "negotiate" => {
                // Avoid allocating the full tool_definitions() vec just to count.
                let tool_count = TOOL_COUNT;
                Ok(serde_json::json!({
                    "protocol_version": crate::models::PROTOCOL_VERSION,
                    "features": [
                        "compression",
                        "toon",
                        "batch",
                        "sse-routing",
                        "ack-tracking",
                        "pending-acks",
                        "lz4-body-compression",
                        "channels",
                        "ownership-arbitration"
                    ],
                    "transports": ["stdio", "http", "mcp-http"],
                    "schemas": ["finding", "status", "benchmark"],
                    "encoding_formats": ["json", "compact", "human", "toon"],
                    "server_version": env!("CARGO_PKG_VERSION"),
                    "mcp_spec_version": "2024-11-05",
                    "tools": tool_count,
                }))
            }

            "create_channel" => {
                let channel_type = get_str(args, "channel_type")
                    .ok_or_else(|| crate::error::AgentBusError::Internal(format!("channel_type is required")))?;
                let created_by =
                    get_str(args, "created_by").ok_or_else(|| crate::error::AgentBusError::Internal(format!("created_by is required")))?;

                match channel_type {
                    "group" => {
                        let name = get_str(args, "name")
                            .ok_or_else(|| crate::error::AgentBusError::Internal(format!("name is required for group channels")))?;
                        let members = get_string_array(args, "members");
                        let info = ops_create_group(
                            settings,
                            &CreateGroupRequest {
                                name,
                                members: &members,
                                created_by,
                            },
                        )?;
                        Ok(serde_json::to_value(&info)?)
                    }
                    other => Err(crate::error::AgentBusError::Internal(format!(
                        "unsupported channel_type '{other}' for create_channel; supported: group"
                    ))),
                }
            }

            "post_to_channel" => {
                let channel_type = get_str(args, "channel_type")
                    .ok_or_else(|| crate::error::AgentBusError::Internal(format!("channel_type is required")))?;
                let sender =
                    get_str(args, "sender").ok_or_else(|| crate::error::AgentBusError::Internal(format!("sender is required")))?;
                let body = get_str(args, "body").ok_or_else(|| crate::error::AgentBusError::Internal(format!("body is required")))?;
                let topic = get_str_or(args, "topic", channel_type);
                let thread_id = get_str(args, "thread_id");
                let tags = get_string_array(args, "tags");

                match channel_type {
                    "direct" => {
                        let recipient = get_str(args, "recipient")
                            .ok_or_else(|| crate::error::AgentBusError::Internal(format!("recipient is required for direct channels")))?;
                        let msg = ops_post_direct(
                            settings,
                            &PostDirectRequest {
                                from_agent: sender,
                                to_agent: recipient,
                                topic,
                                body,
                                thread_id,
                                tags: &tags,
                            },
                        )?;
                        Ok(serde_json::to_value(&msg)?)
                    }
                    "group" => {
                        let group_name = get_str(args, "recipient").ok_or_else(|| {
                            crate::error::AgentBusError::Internal(format!("recipient (group name) is required for group channels"))
                        })?;
                        let msg = ops_post_group(
                            settings,
                            &PostGroupRequest {
                                group: group_name,
                                from_agent: sender,
                                topic,
                                body,
                                thread_id,
                            },
                        )?;
                        Ok(serde_json::to_value(&msg)?)
                    }
                    "escalate" => {
                        let msg = ops_post_escalation(
                            settings,
                            &EscalateRequest {
                                from_agent: sender,
                                body,
                                thread_id,
                                tags: &tags,
                            },
                        )?;
                        Ok(serde_json::to_value(&msg)?)
                    }
                    other => Err(crate::error::AgentBusError::Internal(format!(
                        "unsupported channel_type '{other}'; supported: direct, group, escalate"
                    ))),
                }
            }

            "read_channel" => {
                let channel_type = get_str(args, "channel_type")
                    .ok_or_else(|| crate::error::AgentBusError::Internal(format!("channel_type is required")))?;
                let limit = get_usize_or(args, "limit", 50);

                match channel_type {
                    "direct" => {
                        let agent_a = get_str(args, "agent_a")
                            .ok_or_else(|| crate::error::AgentBusError::Internal(format!("agent_a is required for direct channels")))?;
                        let agent_b = get_str(args, "agent_b")
                            .ok_or_else(|| crate::error::AgentBusError::Internal(format!("agent_b is required for direct channels")))?;
                        let msgs = ops_read_direct(
                            settings,
                            &ReadDirectRequest {
                                agent_a,
                                agent_b,
                                limit,
                            },
                        )?;
                        Ok(serde_json::to_value(&msgs)?)
                    }
                    "group" => {
                        let group_name = get_str(args, "group_name")
                            .ok_or_else(|| crate::error::AgentBusError::Internal(format!("group_name is required for group channels")))?;
                        let msgs = ops_read_group(
                            settings,
                            &ReadGroupRequest {
                                group: group_name,
                                limit,
                            },
                        )?;
                        Ok(serde_json::to_value(&msgs)?)
                    }
                    other => Err(crate::error::AgentBusError::Internal(format!(
                        "unsupported channel_type '{other}' for read_channel; supported: direct, group"
                    ))),
                }
            }

            "claim_resource" => {
                let resource =
                    get_str(args, "resource").ok_or_else(|| crate::error::AgentBusError::Internal(format!("resource is required")))?;
                let agent = get_str(args, "agent").ok_or_else(|| crate::error::AgentBusError::Internal(format!("agent is required")))?;
                let reason = get_str_or(args, "reason", "first-edit required");
                let mode = get_str_or(args, "mode", "exclusive");
                let namespace = get_str(args, "namespace").map(str::to_owned);
                let scope_kind = get_str(args, "scope_kind").map(str::to_owned);
                let scope_path = get_str(args, "scope_path").map(str::to_owned);
                let repo_scopes = get_string_array(args, "repo_scopes");
                let thread_id = get_str(args, "thread_id").map(str::to_owned);
                let lease_ttl_seconds = get_u64_or(args, "lease_ttl_seconds", 3600);
                let scope = get_str(args, "scope")
                    .map(parse_resource_scope)
                    .transpose()?;

                let claim = ops_claim_resource(
                    settings,
                    &ClaimResourceRequest {
                        resource,
                        agent,
                        reason,
                        mode,
                        namespace: namespace.as_deref(),
                        scope_kind: scope_kind.as_deref(),
                        scope_path: scope_path.as_deref(),
                        repo_scopes: &repo_scopes,
                        thread_id: thread_id.as_deref(),
                        lease_ttl_seconds,
                        scope,
                    },
                )?;
                Ok(serde_json::to_value(&claim)?)
            }

            "renew_claim" => {
                let resource =
                    get_str(args, "resource").ok_or_else(|| crate::error::AgentBusError::Internal(format!("resource is required")))?;
                let agent = get_str(args, "agent").ok_or_else(|| crate::error::AgentBusError::Internal(format!("agent is required")))?;
                let lease_ttl_seconds = get_u64_or(args, "lease_ttl_seconds", 3600);
                let claim = ops_renew_claim(
                    settings,
                    &RenewClaimRequest {
                        resource,
                        agent,
                        lease_ttl_seconds,
                    },
                )?;
                Ok(serde_json::to_value(&claim)?)
            }

            "release_claim" => {
                let resource =
                    get_str(args, "resource").ok_or_else(|| crate::error::AgentBusError::Internal(format!("resource is required")))?;
                let agent = get_str(args, "agent").ok_or_else(|| crate::error::AgentBusError::Internal(format!("agent is required")))?;
                let state = ops_release_claim(settings, &ReleaseClaimRequest { resource, agent })?;
                Ok(serde_json::to_value(&state)?)
            }

            "resolve_claim" => {
                let resource =
                    get_str(args, "resource").ok_or_else(|| crate::error::AgentBusError::Internal(format!("resource is required")))?;
                let winner =
                    get_str(args, "winner").ok_or_else(|| crate::error::AgentBusError::Internal(format!("winner is required")))?;
                let reason = get_str_or(args, "reason", "resolved by orchestrator");
                let resolved_by = get_str_or(args, "resolved_by", "orchestrator");

                let state = ops_resolve_claim(
                    settings,
                    &ResolveClaimRequest {
                        resource,
                        winner,
                        reason,
                        resolved_by,
                    },
                )?;
                Ok(serde_json::to_value(&state)?)
            }

            "knock_agent" => {
                let sender =
                    get_str(args, "sender").ok_or_else(|| crate::error::AgentBusError::Internal(format!("sender is required")))?;
                let recipient =
                    get_str(args, "recipient").ok_or_else(|| crate::error::AgentBusError::Internal(format!("recipient is required")))?;
                let body = get_str_or(args, "body", "check the bus");
                let thread_id = get_str(args, "thread_id");
                let tags = get_string_array(args, "tags");
                let request_ack = get_bool_or(args, "request_ack", true);
                let metadata = knock_metadata(request_ack);

                let mut conn = connect(settings)?;
                let msg = post_message(
                    &mut conn,
                    settings,
                    &PostMessageRequest {
                        sender,
                        recipient,
                        topic: "knock",
                        body,
                        thread_id,
                        tags: &tags,
                        priority: "urgent",
                        request_ack,
                        reply_to: None,
                        metadata: &metadata,
                        has_sse_subscribers: false,
                    },
                )?;
                Ok(serde_json::to_value(&msg)?)
            }

            "check_inbox" => {
                let agent = get_str(args, "agent").ok_or_else(|| crate::error::AgentBusError::Internal(format!("agent is required")))?;
                let repo = get_str(args, "repo");
                let session = get_str(args, "session");
                let tags = get_string_array(args, "tag");
                let thread_id = get_str(args, "thread_id");
                let limit = get_usize_or(args, "limit", 10).min(100);
                let reset_cursor = get_bool_or(args, "reset_cursor", false);

                let mut conn = connect(settings)?;
                let result = check_inbox(
                    &mut conn,
                    &CheckInboxRequest {
                        agent,
                        limit,
                        reset_cursor,
                        filters: MessageFilters {
                            repo,
                            session,
                            tags: &tags,
                            thread_id,
                        },
                    },
                )?;

                Ok(serde_json::json!({
                    "agent": agent,
                    "new_messages": result.messages.len(),
                    "messages": result.messages,
                    "cursor_was": result.cursor_was,
                    "cursor_now": result.cursor_now,
                    "inbox_cursor_key": result.inbox_cursor_key,
                }))
            }

            other => Err(crate::error::AgentBusError::Internal(format!("unknown tool: {other}"))),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_has_expected_count() {
        let tools = tool_definitions();
        assert_eq!(
            tools.len(),
            TOOL_COUNT,
            "tool_definitions vs TOOL_COUNT mismatch"
        );
    }

    #[test]
    fn tool_definition_names_are_correct() {
        let tools = tool_definitions();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"bus_health"));
        assert!(names.contains(&"post_message"));
        assert!(names.contains(&"list_messages"));
        assert!(names.contains(&"ack_message"));
        assert!(names.contains(&"set_presence"));
        assert!(names.contains(&"list_presence"));
        assert!(names.contains(&"list_presence_history"));
        assert!(names.contains(&"negotiate"));
        assert!(names.contains(&"create_channel"));
        assert!(names.contains(&"post_to_channel"));
        assert!(names.contains(&"read_channel"));
        assert!(names.contains(&"claim_resource"));
        assert!(names.contains(&"renew_claim"));
        assert!(names.contains(&"release_claim"));
        assert!(names.contains(&"resolve_claim"));
        assert!(names.contains(&"knock_agent"));
        assert!(names.contains(&"check_inbox"));
    }

    #[test]
    fn all_tool_definitions_have_non_empty_description() {
        for tool in tool_definitions() {
            assert!(
                !tool.description.is_empty(),
                "tool '{}' has an empty description",
                tool.name
            );
        }
    }

    #[test]
    fn all_tool_schemas_declare_type_object() {
        for tool in tool_definitions() {
            let schema_type = tool.schema.get("type").and_then(|v| v.as_str());
            assert_eq!(
                schema_type,
                Some("object"),
                "tool '{}' schema must have \"type\": \"object\"",
                tool.name
            );
        }
    }

    #[test]
    fn post_message_schema_has_required_fields() {
        let tools = tool_definitions();
        let tool = tools
            .iter()
            .find(|t| t.name == "post_message")
            .expect("post_message must exist");

        let required = tool
            .schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("post_message schema must have a 'required' array");

        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        for field in &["sender", "recipient", "topic", "body"] {
            assert!(
                required_names.contains(field),
                "post_message 'required' must include '{field}', got: {required_names:?}"
            );
        }
    }

    #[test]
    fn schema_for_empty_required_omits_required_key() {
        let schema = schemas::bus_health();
        assert!(
            schema.get("required").is_none(),
            "bus_health schema must not have 'required' key when no fields are required"
        );

        let schema = schemas::list_presence();
        assert!(
            schema.get("required").is_none(),
            "list_presence schema must not have 'required' key"
        );

        let schema = schemas::negotiate();
        assert!(
            schema.get("required").is_none(),
            "negotiate schema must not have 'required' key"
        );
    }

    #[test]
    fn negotiate_dispatches_without_redis() {
        let settings = crate::settings::Settings::from_env();
        let dispatch = McpToolDispatch::new(&settings);
        let result = dispatch.dispatch_tool("negotiate", &serde_json::Map::new());
        let val = result.expect("negotiate tool failed");
        assert_eq!(val["protocol_version"], crate::models::PROTOCOL_VERSION);
        assert!(val["transports"].as_array().is_some());
        assert!(val["schemas"].as_array().is_some());
    }

    #[test]
    fn dispatch_unknown_tool_returns_error() {
        let settings = crate::settings::Settings::from_env();
        let dispatch = McpToolDispatch::new(&settings);
        let result = dispatch.dispatch_tool("nonexistent_tool", &serde_json::Map::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown tool"));
    }

    #[test]
    fn negotiate_lists_channel_features() {
        let settings = crate::settings::Settings::from_env();
        let dispatch = McpToolDispatch::new(&settings);
        let result = dispatch.dispatch_tool("negotiate", &serde_json::Map::new());
        let val = result.expect("negotiate tool failed");
        let features = val["features"].as_array().expect("features must be array");
        let has_channels = features.iter().any(|f| f.as_str() == Some("channels"));
        let has_arbitration = features
            .iter()
            .any(|f| f.as_str() == Some("ownership-arbitration"));
        assert!(has_channels, "negotiate should list 'channels' feature");
        assert!(
            has_arbitration,
            "negotiate should list 'ownership-arbitration' feature"
        );
    }

    #[test]
    fn create_channel_requires_channel_type() {
        let settings = crate::settings::Settings::from_env();
        let dispatch = McpToolDispatch::new(&settings);
        let args = serde_json::json!({"created_by": "claude"});
        let result = dispatch.dispatch_tool(
            "create_channel",
            args.as_object().expect("args must be object"),
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("channel_type"),
            "error should mention channel_type, got: {msg}"
        );
    }

    #[test]
    fn post_to_channel_rejects_unknown_channel_type() {
        let settings = crate::settings::Settings::from_env();
        let dispatch = McpToolDispatch::new(&settings);
        let args = serde_json::json!({
            "channel_type": "invalid",
            "sender": "claude",
            "body": "hello"
        });
        let result = dispatch.dispatch_tool(
            "post_to_channel",
            args.as_object().expect("args must be object"),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unsupported"));
    }

    #[test]
    fn claim_resource_requires_resource_and_agent() {
        let settings = crate::settings::Settings::from_env();
        let dispatch = McpToolDispatch::new(&settings);

        let args1 = serde_json::json!({"resource": "src/main.rs"});
        let r1 = dispatch.dispatch_tool("claim_resource", args1.as_object().unwrap());
        assert!(r1.is_err());

        let args2 = serde_json::json!({"agent": "claude"});
        let r2 = dispatch.dispatch_tool("claim_resource", args2.as_object().unwrap());
        assert!(r2.is_err());
    }

    #[test]
    fn claim_resource_schema_includes_scope_enum() {
        let tool = tool_definitions()
            .into_iter()
            .find(|tool| tool.name == "claim_resource")
            .expect("claim_resource must exist");

        let scope = tool
            .schema
            .get("properties")
            .and_then(|v| v.as_object())
            .and_then(|props| props.get("scope"))
            .expect("claim_resource schema must expose scope");

        let enum_values: Vec<&str> = scope["enum"]
            .as_array()
            .expect("scope enum must exist")
            .iter()
            .filter_map(|value| value.as_str())
            .collect();

        assert_eq!(enum_values, vec!["repo", "machine"]);
    }

    #[test]
    fn claim_resource_rejects_invalid_scope_before_backend_access() {
        let settings = crate::settings::Settings::from_env();
        let dispatch = McpToolDispatch::new(&settings);
        let args = serde_json::json!({
            "resource": "src/main.rs",
            "agent": "claude",
            "scope": "planet"
        });

        let err = dispatch
            .dispatch_tool(
                "claim_resource",
                args.as_object().expect("args must be object"),
            )
            .expect_err("invalid scope must fail");

        assert!(
            !err.to_string().trim().is_empty(),
            "invalid scope error should not be empty"
        );
    }

    #[test]
    fn read_channel_group_requires_group_name_before_backend_access() {
        let settings = crate::settings::Settings::from_env();
        let dispatch = McpToolDispatch::new(&settings);
        let args = serde_json::json!({
            "channel_type": "group",
            "limit": 5
        });

        let err = dispatch
            .dispatch_tool(
                "read_channel",
                args.as_object().expect("args must be object"),
            )
            .expect_err("missing group_name must fail");

        assert!(err.to_string().contains("group_name"));
    }

    #[test]
    fn check_inbox_requires_agent_arg() {
        let settings = crate::settings::Settings::from_env();
        let dispatch = McpToolDispatch::new(&settings);
        let args = serde_json::json!({"limit": 5});
        let result = dispatch.dispatch_tool(
            "check_inbox",
            args.as_object().expect("args must be object"),
        );
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("agent"),
            "error must mention 'agent', got: {msg}"
        );
    }

    #[test]
    fn check_inbox_schema_requires_agent() {
        let tools = tool_definitions();
        let tool = tools
            .iter()
            .find(|t| t.name == "check_inbox")
            .expect("check_inbox must exist");

        let required = tool
            .schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("check_inbox schema must have a 'required' array");

        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(required_names.contains(&"agent"));
        assert!(!required_names.contains(&"limit"));
        assert!(!required_names.contains(&"reset_cursor"));
        assert!(!required_names.contains(&"repo"));
        assert!(!required_names.contains(&"session"));
        assert!(!required_names.contains(&"tag"));
        assert!(!required_names.contains(&"thread_id"));
    }

    #[test]
    fn check_inbox_schema_optional_properties_present() {
        let tools = tool_definitions();
        let tool = tools
            .iter()
            .find(|t| t.name == "check_inbox")
            .expect("check_inbox must exist");

        let props = tool
            .schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("check_inbox schema must have 'properties'");

        assert!(props.contains_key("limit"));
        assert!(props.contains_key("reset_cursor"));
        assert!(props.contains_key("repo"));
        assert!(props.contains_key("session"));
        assert!(props.contains_key("tag"));
        assert!(props.contains_key("thread_id"));
    }
}
