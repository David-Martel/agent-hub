//! MCP server handler exposing agent-bus operations as MCP tools.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};

use crate::ops::{
    AckMessageRequest, MessageFilters, PostMessageRequest, PresenceRequest, ReadMessagesRequest,
    knock_metadata, list_messages_history, post_ack, post_message, set_presence,
};
use crate::ops::channel::{
    CreateGroupRequest, EscalateRequest, PostDirectRequest, PostGroupRequest, ReadDirectRequest,
    ReadGroupRequest, create_group as ops_create_group, post_direct as ops_post_direct,
    post_escalation as ops_post_escalation, post_group as ops_post_group,
    read_direct as ops_read_direct, read_group as ops_read_group,
};
use crate::ops::claim::{
    ClaimResourceRequest, ReleaseClaimRequest, RenewClaimRequest, ResolveClaimRequest,
    claim_resource as ops_claim_resource, release_claim as ops_release_claim,
    renew_claim as ops_renew_claim, resolve_claim as ops_resolve_claim,
};
use crate::ops::inbox::{CheckInboxRequest, check_inbox};
use crate::redis_bus::{bus_health, bus_list_presence, connect};
use crate::settings::Settings;
use crate::validation::{
    auto_fit_schema, enforce_schema_for_transport, validate_message_schema, validate_priority,
};

/// Tool input schemas, separated from execution logic.
mod schemas {
    use std::sync::Arc;

    pub(super) fn schema_for(
        props: serde_json::Value,
        required: &[&str],
    ) -> Arc<serde_json::Map<String, serde_json::Value>> {
        let mut schema = serde_json::Map::new();
        schema.insert("type".to_owned(), serde_json::json!("object"));
        schema.insert("properties".to_owned(), props);
        if !required.is_empty() {
            schema.insert(
                "required".to_owned(),
                serde_json::Value::Array(required.iter().map(|s| serde_json::json!(s)).collect()),
            );
        }
        Arc::new(schema)
    }

    pub(super) fn bus_health() -> Arc<serde_json::Map<String, serde_json::Value>> {
        schema_for(serde_json::json!({}), &[])
    }

    pub(super) fn post_message() -> Arc<serde_json::Map<String, serde_json::Value>> {
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

    pub(super) fn list_messages() -> Arc<serde_json::Map<String, serde_json::Value>> {
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

    pub(super) fn ack_message() -> Arc<serde_json::Map<String, serde_json::Value>> {
        schema_for(
            serde_json::json!({
                "agent":      {"type": "string"},
                "message_id": {"type": "string"},
                "body":       {"type": "string"}
            }),
            &["agent", "message_id"],
        )
    }

    pub(super) fn set_presence() -> Arc<serde_json::Map<String, serde_json::Value>> {
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

    pub(super) fn list_presence() -> Arc<serde_json::Map<String, serde_json::Value>> {
        schema_for(serde_json::json!({}), &[])
    }

    pub(super) fn list_presence_history() -> Arc<serde_json::Map<String, serde_json::Value>> {
        schema_for(
            serde_json::json!({
                "agent":         {"type": "string"},
                "since_minutes": {"type": "integer", "minimum": 1, "maximum": 10080},
                "limit":         {"type": "integer", "minimum": 1, "maximum": 500}
            }),
            &[],
        )
    }

    pub(super) fn negotiate() -> Arc<serde_json::Map<String, serde_json::Value>> {
        schema_for(serde_json::json!({}), &[])
    }

    pub(super) fn create_channel() -> Arc<serde_json::Map<String, serde_json::Value>> {
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

    pub(super) fn post_to_channel() -> Arc<serde_json::Map<String, serde_json::Value>> {
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

    pub(super) fn read_channel() -> Arc<serde_json::Map<String, serde_json::Value>> {
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

    pub(super) fn claim_resource() -> Arc<serde_json::Map<String, serde_json::Value>> {
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
                "lease_ttl_seconds": {"type": "integer", "minimum": 1, "maximum": 86400}
            }),
            &["resource", "agent"],
        )
    }

    pub(super) fn renew_claim() -> Arc<serde_json::Map<String, serde_json::Value>> {
        schema_for(
            serde_json::json!({
                "resource": {"type": "string"},
                "agent": {"type": "string"},
                "lease_ttl_seconds": {"type": "integer", "minimum": 1, "maximum": 86400}
            }),
            &["resource", "agent"],
        )
    }

    pub(super) fn release_claim() -> Arc<serde_json::Map<String, serde_json::Value>> {
        schema_for(
            serde_json::json!({
                "resource": {"type": "string"},
                "agent": {"type": "string"}
            }),
            &["resource", "agent"],
        )
    }

    pub(super) fn resolve_claim() -> Arc<serde_json::Map<String, serde_json::Value>> {
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

    pub(super) fn check_inbox() -> Arc<serde_json::Map<String, serde_json::Value>> {
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

    pub(super) fn knock_agent() -> Arc<serde_json::Map<String, serde_json::Value>> {
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

/// MCP server handler that exposes agent-bus operations as MCP tools.
///
/// Settings are immutable after construction, so no `Mutex` is needed.
pub(crate) struct AgentBusMcpServer {
    settings: Arc<Settings>,
}

impl AgentBusMcpServer {
    pub(crate) fn new(settings: Settings) -> Self {
        Self {
            settings: Arc::new(settings),
        }
    }

    pub(crate) fn tool_list() -> Vec<Tool> {
        vec![
            Tool::new(
                "bus_health",
                "Check the health of the agent-bus Redis backend.",
                schemas::bus_health(),
            ),
            Tool::new(
                "post_message",
                "Post a message to the agent coordination bus.",
                schemas::post_message(),
            ),
            Tool::new(
                "list_messages",
                "List recent messages from the bus, optionally filtered by recipient, sender, repo, session, tag, or thread_id.",
                schemas::list_messages(),
            ),
            Tool::new(
                "ack_message",
                "Acknowledge a message by posting an ack reply.",
                schemas::ack_message(),
            ),
            Tool::new(
                "set_presence",
                "Announce or update agent presence on the bus.",
                schemas::set_presence(),
            ),
            Tool::new(
                "list_presence",
                "List all active agent presence records.",
                schemas::list_presence(),
            ),
            Tool::new(
                "list_presence_history",
                "Query PostgreSQL for historical presence events within a time window.",
                schemas::list_presence_history(),
            ),
            Tool::new(
                "negotiate",
                "Return server capabilities: protocol version, supported features, \
                 transports, schemas, and encoding formats.",
                schemas::negotiate(),
            ),
            Tool::new(
                "create_channel",
                "Create a new group channel with an initial member list.",
                schemas::create_channel(),
            ),
            Tool::new(
                "post_to_channel",
                "Post a message to a direct, group, or escalation channel.",
                schemas::post_to_channel(),
            ),
            Tool::new(
                "read_channel",
                "Read messages from a direct or group channel.",
                schemas::read_channel(),
            ),
            Tool::new(
                "claim_resource",
                "Claim first-edit ownership of a resource. First claim is auto-granted; \
                 contested claims trigger orchestrator escalation.",
                schemas::claim_resource(),
            ),
            Tool::new(
                "renew_claim",
                "Renew an active resource claim/lease before it expires.",
                schemas::renew_claim(),
            ),
            Tool::new(
                "release_claim",
                "Release an active resource claim/lease held by an agent.",
                schemas::release_claim(),
            ),
            Tool::new(
                "resolve_claim",
                "Resolve a contested ownership claim by naming a winner and sending \
                 direct notifications to all claimants.",
                schemas::resolve_claim(),
            ),
            Tool::new(
                "knock_agent",
                "Send a durable direct knock notification to make another agent check the bus now.",
                schemas::knock_agent(),
            ),
            Tool::new(
                "check_inbox",
                "Return new messages for an agent since the last check (cursor-based). \
                 The cursor advances automatically so repeated calls yield only new messages. \
                 Use reset_cursor=true to re-read from the beginning of the stream.",
                schemas::check_inbox(),
            ),
        ]
    }

    /// Synchronous tool dispatch used by the MCP Streamable HTTP transport.
    ///
    /// Mirrors [`call_tool`] but returns a plain `serde_json::Value` rather than
    /// `CallToolResult`, making it usable in blocking tasks spawned by the HTTP
    /// handler without requiring the full `rmcp` async machinery.
    ///
    /// # Errors
    ///
    /// Returns an error if the tool name is unknown or the tool logic fails.
    pub(crate) fn call_tool_sync(
        &self,
        name: &str,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value> {
        self.handle_tool_call_inner(name, args)
    }

    fn json_to_text(value: &serde_json::Value) -> Content {
        Content::text(serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned()))
    }

    fn err_content(e: &anyhow::Error) -> CallToolResult {
        CallToolResult::error(vec![Content::text(format!("Error: {e:#}"))])
    }

    fn ok_content(value: &serde_json::Value) -> CallToolResult {
        CallToolResult::success(vec![Self::json_to_text(value)])
    }

    /// Extract a string arg from `CallToolRequestParams`.
    fn get_str<'a>(
        params: &'a serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> Option<&'a str> {
        params.get(key)?.as_str()
    }

    /// Extract a string arg with a default.
    fn get_str_or<'a>(
        params: &'a serde_json::Map<String, serde_json::Value>,
        key: &str,
        default: &'a str,
    ) -> &'a str {
        params.get(key).and_then(|v| v.as_str()).unwrap_or(default)
    }

    fn get_bool_or(
        params: &serde_json::Map<String, serde_json::Value>,
        key: &str,
        default: bool,
    ) -> bool {
        params
            .get(key)
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(default)
    }

    fn get_u64_or(
        params: &serde_json::Map<String, serde_json::Value>,
        key: &str,
        default: u64,
    ) -> u64 {
        params
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(default)
    }

    fn get_usize_or(
        params: &serde_json::Map<String, serde_json::Value>,
        key: &str,
        default: usize,
    ) -> usize {
        params
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .unwrap_or(default)
    }

    fn get_string_array(
        params: &serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> Vec<String> {
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

    fn get_object_or_empty(
        params: &serde_json::Map<String, serde_json::Value>,
        key: &str,
    ) -> serde_json::Value {
        params
            .get(key)
            .filter(|v| v.is_object())
            .cloned()
            .unwrap_or(serde_json::Value::Object(serde_json::Map::new()))
    }

    #[expect(
        clippy::too_many_lines,
        reason = "match on 7 tool names, each short — extracting further would reduce clarity"
    )]
    fn handle_tool_call_inner(
        &self,
        name: &str,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let settings = &*self.settings;

        match name {
            "bus_health" => {
                // MCP stdio transport has no r2d2 pool; pass None so bus_health
                // falls back to a single fresh connection.
                let h = bus_health(settings, None);
                Ok(serde_json::to_value(&h)?)
            }

            "post_message" => {
                let sender =
                    Self::get_str(args, "sender").ok_or_else(|| anyhow!("sender is required"))?;
                let recipient = Self::get_str(args, "recipient")
                    .ok_or_else(|| anyhow!("recipient is required"))?;
                let topic =
                    Self::get_str(args, "topic").ok_or_else(|| anyhow!("topic is required"))?;
                let body =
                    Self::get_str(args, "body").ok_or_else(|| anyhow!("body is required"))?;
                let tags = Self::get_string_array(args, "tags");
                let thread_id = Self::get_str(args, "thread_id");
                let priority = Self::get_str_or(args, "priority", "normal");
                let request_ack = Self::get_bool_or(args, "request_ack", false);
                let reply_to = Self::get_str(args, "reply_to");
                let metadata = Self::get_object_or_empty(args, "metadata");
                let schema = Self::get_str(args, "schema");
                let effective_schema = enforce_schema_for_transport("mcp", schema, topic);

                validate_priority(priority)?;
                let fitted_body = auto_fit_schema(body, effective_schema);
                validate_message_schema(&fitted_body, effective_schema)?;
                let mut conn = connect(settings)?;
                let msg = post_message(
                    &mut conn,
                    settings,
                    &PostMessageRequest {
                        sender,
                        recipient,
                        topic,
                        body: &fitted_body,
                        thread_id,
                        tags: &tags,
                        priority,
                        request_ack,
                        reply_to,
                        metadata: &metadata,
                        has_sse_subscribers: false,
                    },
                )?;
                Ok(serde_json::to_value(&msg)?)
            }

            "list_messages" => {
                let agent = Self::get_str(args, "agent");
                let sender = Self::get_str(args, "sender");
                let repo = Self::get_str(args, "repo");
                let session = Self::get_str(args, "session");
                let tags = Self::get_string_array(args, "tag");
                let thread_id = Self::get_str(args, "thread_id");
                let since_minutes = Self::get_u64_or(args, "since_minutes", 1440);
                let limit = Self::get_usize_or(args, "limit", 50);
                let include_broadcast = Self::get_bool_or(args, "include_broadcast", true);
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
                let agent =
                    Self::get_str(args, "agent").ok_or_else(|| anyhow!("agent is required"))?;
                let message_id = Self::get_str(args, "message_id")
                    .ok_or_else(|| anyhow!("message_id is required"))?;
                let body = Self::get_str_or(args, "body", "ack");

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
                let agent =
                    Self::get_str(args, "agent").ok_or_else(|| anyhow!("agent is required"))?;
                let status = Self::get_str_or(args, "status", "online");
                let session_id = Self::get_str(args, "session_id");
                let capabilities = Self::get_string_array(args, "capabilities");
                let ttl_seconds = Self::get_u64_or(args, "ttl_seconds", 180);
                let metadata = Self::get_object_or_empty(args, "metadata");

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
                let results = bus_list_presence(&mut conn, settings)?;
                Ok(serde_json::to_value(&results)?)
            }

            "list_presence_history" => {
                let agent = Self::get_str(args, "agent");
                let since_minutes = Self::get_u64_or(args, "since_minutes", 1440);
                let limit = Self::get_usize_or(args, "limit", 50);

                let events = crate::postgres_store::list_presence_history_postgres(
                    settings,
                    agent,
                    since_minutes,
                    limit,
                )?;
                Ok(serde_json::to_value(&events)?)
            }

            "negotiate" => Ok(serde_json::json!({
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
                "tools": AgentBusMcpServer::tool_list().len(),
            })),

            "create_channel" => {
                let channel_type = Self::get_str(args, "channel_type")
                    .ok_or_else(|| anyhow!("channel_type is required"))?;
                let created_by = Self::get_str(args, "created_by")
                    .ok_or_else(|| anyhow!("created_by is required"))?;

                match channel_type {
                    "group" => {
                        let name = Self::get_str(args, "name")
                            .ok_or_else(|| anyhow!("name is required for group channels"))?;
                        let members = Self::get_string_array(args, "members");
                        let info = ops_create_group(
                            settings,
                            &CreateGroupRequest { name, members: &members, created_by },
                        )?;
                        Ok(serde_json::to_value(&info)?)
                    }
                    other => Err(anyhow!(
                        "unsupported channel_type '{other}' for create_channel; supported: group"
                    )),
                }
            }

            "post_to_channel" => {
                let channel_type = Self::get_str(args, "channel_type")
                    .ok_or_else(|| anyhow!("channel_type is required"))?;
                let sender =
                    Self::get_str(args, "sender").ok_or_else(|| anyhow!("sender is required"))?;
                let body =
                    Self::get_str(args, "body").ok_or_else(|| anyhow!("body is required"))?;
                let topic = Self::get_str_or(args, "topic", channel_type);
                let thread_id = Self::get_str(args, "thread_id");
                let tags = Self::get_string_array(args, "tags");

                match channel_type {
                    "direct" => {
                        let recipient = Self::get_str(args, "recipient")
                            .ok_or_else(|| anyhow!("recipient is required for direct channels"))?;
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
                        let group_name = Self::get_str(args, "recipient").ok_or_else(|| {
                            anyhow!("recipient (group name) is required for group channels")
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
                            &EscalateRequest { from_agent: sender, body, thread_id, tags: &tags },
                        )?;
                        Ok(serde_json::to_value(&msg)?)
                    }
                    other => Err(anyhow!(
                        "unsupported channel_type '{other}'; supported: direct, group, escalate"
                    )),
                }
            }

            "read_channel" => {
                let channel_type = Self::get_str(args, "channel_type")
                    .ok_or_else(|| anyhow!("channel_type is required"))?;
                let limit = Self::get_usize_or(args, "limit", 50);

                match channel_type {
                    "direct" => {
                        let agent_a = Self::get_str(args, "agent_a")
                            .ok_or_else(|| anyhow!("agent_a is required for direct channels"))?;
                        let agent_b = Self::get_str(args, "agent_b")
                            .ok_or_else(|| anyhow!("agent_b is required for direct channels"))?;
                        let msgs = ops_read_direct(
                            settings,
                            &ReadDirectRequest { agent_a, agent_b, limit },
                        )?;
                        Ok(serde_json::to_value(&msgs)?)
                    }
                    "group" => {
                        let group_name = Self::get_str(args, "group_name")
                            .ok_or_else(|| anyhow!("group_name is required for group channels"))?;
                        let msgs =
                            ops_read_group(settings, &ReadGroupRequest { group: group_name, limit })?;
                        Ok(serde_json::to_value(&msgs)?)
                    }
                    other => Err(anyhow!(
                        "unsupported channel_type '{other}' for read_channel; supported: direct, group"
                    )),
                }
            }

            "claim_resource" => {
                let resource = Self::get_str(args, "resource")
                    .ok_or_else(|| anyhow!("resource is required"))?;
                let agent =
                    Self::get_str(args, "agent").ok_or_else(|| anyhow!("agent is required"))?;
                let reason = Self::get_str_or(args, "reason", "first-edit required");
                let mode = Self::get_str_or(args, "mode", "exclusive");
                let namespace = Self::get_str(args, "namespace").map(str::to_owned);
                let scope_kind = Self::get_str(args, "scope_kind").map(str::to_owned);
                let scope_path = Self::get_str(args, "scope_path").map(str::to_owned);
                let repo_scopes = Self::get_string_array(args, "repo_scopes");
                let thread_id = Self::get_str(args, "thread_id").map(str::to_owned);
                let lease_ttl_seconds = Self::get_u64_or(args, "lease_ttl_seconds", 3600);

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
                    },
                )?;
                Ok(serde_json::to_value(&claim)?)
            }

            "renew_claim" => {
                let resource = Self::get_str(args, "resource")
                    .ok_or_else(|| anyhow!("resource is required"))?;
                let agent =
                    Self::get_str(args, "agent").ok_or_else(|| anyhow!("agent is required"))?;
                let lease_ttl_seconds = Self::get_u64_or(args, "lease_ttl_seconds", 3600);
                let claim = ops_renew_claim(
                    settings,
                    &RenewClaimRequest { resource, agent, lease_ttl_seconds },
                )?;
                Ok(serde_json::to_value(&claim)?)
            }

            "release_claim" => {
                let resource = Self::get_str(args, "resource")
                    .ok_or_else(|| anyhow!("resource is required"))?;
                let agent =
                    Self::get_str(args, "agent").ok_or_else(|| anyhow!("agent is required"))?;
                let state = ops_release_claim(
                    settings,
                    &ReleaseClaimRequest { resource, agent },
                )?;
                Ok(serde_json::to_value(&state)?)
            }

            "resolve_claim" => {
                let resource = Self::get_str(args, "resource")
                    .ok_or_else(|| anyhow!("resource is required"))?;
                let winner =
                    Self::get_str(args, "winner").ok_or_else(|| anyhow!("winner is required"))?;
                let reason = Self::get_str_or(args, "reason", "resolved by orchestrator");
                let resolved_by = Self::get_str_or(args, "resolved_by", "orchestrator");

                let state = ops_resolve_claim(
                    settings,
                    &ResolveClaimRequest { resource, winner, reason, resolved_by },
                )?;
                Ok(serde_json::to_value(&state)?)
            }

            "knock_agent" => {
                let sender =
                    Self::get_str(args, "sender").ok_or_else(|| anyhow!("sender is required"))?;
                let recipient = Self::get_str(args, "recipient")
                    .ok_or_else(|| anyhow!("recipient is required"))?;
                let body = Self::get_str_or(args, "body", "check the bus");
                let thread_id = Self::get_str(args, "thread_id");
                let tags = Self::get_string_array(args, "tags");
                let request_ack = Self::get_bool_or(args, "request_ack", true);
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
                        has_sse_subscribers: true,
                    },
                )?;
                Ok(serde_json::to_value(&msg)?)
            }

            "check_inbox" => {
                let agent =
                    Self::get_str(args, "agent").ok_or_else(|| anyhow!("agent is required"))?;
                let repo = Self::get_str(args, "repo");
                let session = Self::get_str(args, "session");
                let tags = Self::get_string_array(args, "tag");
                let thread_id = Self::get_str(args, "thread_id");
                let limit = Self::get_usize_or(args, "limit", 10).min(100);
                let reset_cursor = Self::get_bool_or(args, "reset_cursor", false);

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

            other => Err(anyhow!("unknown tool: {other}")),
        }
    }
}

impl ServerHandler for AgentBusMcpServer {
    fn get_info(&self) -> InitializeResult {
        InitializeResult::new(
            ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(
            Implementation::new("agent-bus", env!("CARGO_PKG_VERSION")),
        )
        .with_instructions(
            "Agent Hub coordination bus (Redis + PostgreSQL). MANDATORY PROTOCOL: \
             (1) set_presence on session start with your agent ID and capabilities. \
             (2) post_message topic=ownership BEFORE editing any file — claim it first, check for conflicts. \
             (3) use check_inbox every 2-3 tool calls — it now reads durable per-agent notifications rather than replaying the raw bus. \
             (4) Schema required: topic *-findings → schema=finding (needs FINDING:+SEVERITY:), \
             topic status/ownership/coordination → schema=status, topic benchmark → schema=benchmark. \
             (5) Batch 3-5 findings per message (max 2000 chars). Include tags=[repo:<name>]. \
             (6) Post COMPLETE summary when done, then poll for follow-up tasks. \
             HTTP alternative: POST http://localhost:8400/messages (faster for subagents without MCP).",
        )
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult {
            tools: Self::tool_list(),
            next_cursor: None,
            meta: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: rmcp::service::RequestContext<rmcp::service::RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let args = request.arguments.as_ref();
        let empty = serde_json::Map::new();
        let args_map = args.unwrap_or(&empty);

        match self.handle_tool_call_inner(&request.name, args_map) {
            Ok(ref value) => Ok(Self::ok_content(value)),
            Err(e) => Ok(Self::err_content(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redis_bus::notification_cursor_key;

    #[test]
    fn tool_list_has_expected_count() {
        let tools = AgentBusMcpServer::tool_list();
        assert_eq!(
            tools.len(),
            17,
            "expected 17 MCP tools after lease renewal/release and knock support"
        );
    }

    #[test]
    fn tool_list_names_are_correct() {
        let tools = AgentBusMcpServer::tool_list();
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        // Original bus tools
        assert!(names.iter().any(|n| n == "bus_health"));
        assert!(names.iter().any(|n| n == "post_message"));
        assert!(names.iter().any(|n| n == "list_messages"));
        assert!(names.iter().any(|n| n == "ack_message"));
        assert!(names.iter().any(|n| n == "set_presence"));
        assert!(names.iter().any(|n| n == "list_presence"));
        assert!(names.iter().any(|n| n == "list_presence_history"));
        assert!(names.iter().any(|n| n == "negotiate"));
        // Channel tools
        assert!(names.iter().any(|n| n == "create_channel"));
        assert!(names.iter().any(|n| n == "post_to_channel"));
        assert!(names.iter().any(|n| n == "read_channel"));
        assert!(names.iter().any(|n| n == "claim_resource"));
        assert!(names.iter().any(|n| n == "resolve_claim"));
        // Inbox notification tool
        assert!(names.iter().any(|n| n == "check_inbox"));
    }

    #[test]
    fn negotiate_returns_protocol_version() {
        let server = AgentBusMcpServer::new(crate::settings::Settings::from_env());
        let result = server.call_tool_sync("negotiate", &serde_json::Map::new());
        let val = result.expect("negotiate tool failed");
        assert_eq!(val["protocol_version"], crate::models::PROTOCOL_VERSION);
        assert!(val["transports"].as_array().is_some());
        assert!(val["schemas"].as_array().is_some());
    }

    #[test]
    fn call_tool_sync_unknown_tool_returns_error() {
        let server = AgentBusMcpServer::new(crate::settings::Settings::from_env());
        let result = server.call_tool_sync("nonexistent_tool", &serde_json::Map::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown tool"));
    }

    #[test]
    fn negotiate_lists_channel_features() {
        let server = AgentBusMcpServer::new(crate::settings::Settings::from_env());
        let result = server.call_tool_sync("negotiate", &serde_json::Map::new());
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
        let server = AgentBusMcpServer::new(crate::settings::Settings::from_env());
        let args = serde_json::json!({"created_by": "claude"});
        let result = server.call_tool_sync(
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
        let server = AgentBusMcpServer::new(crate::settings::Settings::from_env());
        let args = serde_json::json!({
            "channel_type": "invalid",
            "sender": "claude",
            "body": "hello"
        });
        let result = server.call_tool_sync(
            "post_to_channel",
            args.as_object().expect("args must be object"),
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unsupported"));
    }

    #[test]
    fn claim_resource_requires_resource_and_agent() {
        let server = AgentBusMcpServer::new(crate::settings::Settings::from_env());

        // Missing agent
        let args1 = serde_json::json!({"resource": "src/main.rs"});
        let r1 = server.call_tool_sync("claim_resource", args1.as_object().unwrap());
        assert!(r1.is_err());

        // Missing resource
        let args2 = serde_json::json!({"agent": "claude"});
        let r2 = server.call_tool_sync("claim_resource", args2.as_object().unwrap());
        assert!(r2.is_err());
    }

    // ------------------------------------------------------------------
    // Schema inspection — no Redis/PG required
    // ------------------------------------------------------------------

    /// Every tool must carry a non-empty description so LLMs understand its purpose.
    #[test]
    fn all_tools_have_non_empty_description() {
        for tool in AgentBusMcpServer::tool_list() {
            let name = tool.name.as_ref();
            let desc = tool.description.as_deref().unwrap_or("");
            assert!(!desc.is_empty(), "tool '{name}' has an empty description");
        }
    }

    /// Every tool's `input_schema` must declare `"type": "object"` at the root,
    /// satisfying the JSON Schema requirement for MCP tool input objects.
    #[test]
    fn all_tool_schemas_declare_type_object() {
        for tool in AgentBusMcpServer::tool_list() {
            let name = tool.name.as_ref();
            let schema_type = tool.input_schema.get("type").and_then(|v| v.as_str());
            assert_eq!(
                schema_type,
                Some("object"),
                "tool '{name}' input_schema must have \"type\": \"object\""
            );
        }
    }

    /// `post_message` schema must declare the four mandatory fields as required.
    #[test]
    fn post_message_schema_has_required_fields() {
        let tools = AgentBusMcpServer::tool_list();
        let tool = tools
            .iter()
            .find(|t| t.name == "post_message")
            .expect("post_message tool must exist");

        let required = tool
            .input_schema
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

    /// `post_message` schema must declare each required field as a property.
    #[test]
    fn post_message_schema_properties_include_all_required_fields() {
        let tools = AgentBusMcpServer::tool_list();
        let tool = tools
            .iter()
            .find(|t| t.name == "post_message")
            .expect("post_message tool must exist");

        let props = tool
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("post_message schema must have 'properties'");

        for field in &["sender", "recipient", "topic", "body"] {
            assert!(
                props.contains_key(*field),
                "post_message 'properties' must include '{field}'"
            );
        }
    }

    /// `claim_resource` schema must require both `resource` and `agent`.
    #[test]
    fn claim_resource_schema_has_required_fields() {
        let tools = AgentBusMcpServer::tool_list();
        let tool = tools
            .iter()
            .find(|t| t.name == "claim_resource")
            .expect("claim_resource tool must exist");

        let required = tool
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("claim_resource schema must have a 'required' array");

        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();

        assert!(
            required_names.contains(&"resource"),
            "claim_resource 'required' must include 'resource', got: {required_names:?}"
        );
        assert!(
            required_names.contains(&"agent"),
            "claim_resource 'required' must include 'agent', got: {required_names:?}"
        );
    }

    /// `ack_message` schema must require `agent` and `message_id`.
    #[test]
    fn ack_message_schema_has_required_fields() {
        let tools = AgentBusMcpServer::tool_list();
        let tool = tools
            .iter()
            .find(|t| t.name == "ack_message")
            .expect("ack_message tool must exist");

        let required = tool
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("ack_message schema must have a 'required' array");

        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();

        assert!(
            required_names.contains(&"agent"),
            "ack_message 'required' must include 'agent'"
        );
        assert!(
            required_names.contains(&"message_id"),
            "ack_message 'required' must include 'message_id'"
        );
    }

    /// Tools with no required fields (`bus_health`, `list_presence`, `list_messages`,
    /// `list_presence_history`, `negotiate`) must not have a `required` key, or have
    /// an empty array — consistent with how `schema_for` works when `required` is `&[]`.
    #[test]
    fn schema_for_empty_required_omits_required_key() {
        // schema_for(props, &[]) does not insert "required" at all.
        let schema = schemas::bus_health();
        assert!(
            !schema.contains_key("required"),
            "bus_health schema must not have 'required' key when no fields are required"
        );

        let schema = schemas::list_presence();
        assert!(
            !schema.contains_key("required"),
            "list_presence schema must not have 'required' key"
        );

        let schema = schemas::negotiate();
        assert!(
            !schema.contains_key("required"),
            "negotiate schema must not have 'required' key"
        );
    }

    /// `list_messages` must expose the first-class filter fields.
    #[test]
    fn list_messages_schema_exposes_filters() {
        let tool = AgentBusMcpServer::tool_list()
            .into_iter()
            .find(|t| t.name == "list_messages")
            .expect("list_messages tool must exist");

        let props = tool
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("list_messages schema must have properties");

        assert!(props.contains_key("repo"), "list_messages must expose repo");
        assert!(
            props.contains_key("session"),
            "list_messages must expose session"
        );
        assert!(props.contains_key("tag"), "list_messages must expose tag");
        assert!(
            props.contains_key("thread_id"),
            "list_messages must expose thread_id"
        );
    }

    /// `negotiate` tool must be in the list exactly once.
    #[test]
    fn negotiate_tool_appears_exactly_once() {
        let count = AgentBusMcpServer::tool_list()
            .iter()
            .filter(|t| t.name == "negotiate")
            .count();
        assert_eq!(count, 1, "negotiate must appear exactly once in tool_list");
    }

    /// `set_presence` schema must require `agent`.
    #[test]
    fn set_presence_schema_requires_agent() {
        let tools = AgentBusMcpServer::tool_list();
        let tool = tools
            .iter()
            .find(|t| t.name == "set_presence")
            .expect("set_presence tool must exist");

        let required = tool
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("set_presence schema must have a 'required' array");

        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();

        assert!(
            required_names.contains(&"agent"),
            "set_presence 'required' must include 'agent', got: {required_names:?}"
        );
    }

    /// `resolve_claim` schema must require `resource` and `winner`.
    #[test]
    fn resolve_claim_schema_has_required_fields() {
        let tools = AgentBusMcpServer::tool_list();
        let tool = tools
            .iter()
            .find(|t| t.name == "resolve_claim")
            .expect("resolve_claim tool must exist");

        let required = tool
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("resolve_claim schema must have a 'required' array");

        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();

        assert!(
            required_names.contains(&"resource"),
            "resolve_claim 'required' must include 'resource'"
        );
        assert!(
            required_names.contains(&"winner"),
            "resolve_claim 'required' must include 'winner'"
        );
    }

    // ---------------------------------------------------------------------------
    // check_inbox tool — schema and dispatch tests (no Redis required)
    // ---------------------------------------------------------------------------

    /// `check_inbox` schema must require `agent` and keep all filters optional.
    #[test]
    fn check_inbox_schema_requires_agent() {
        let tools = AgentBusMcpServer::tool_list();
        let tool = tools
            .iter()
            .find(|t| t.name == "check_inbox")
            .expect("check_inbox tool must exist");

        let required = tool
            .input_schema
            .get("required")
            .and_then(|v| v.as_array())
            .expect("check_inbox schema must have a 'required' array");

        let required_names: Vec<&str> = required.iter().filter_map(|v| v.as_str()).collect();
        assert!(
            required_names.contains(&"agent"),
            "check_inbox 'required' must include 'agent', got: {required_names:?}"
        );
        assert!(
            !required_names.contains(&"limit"),
            "check_inbox 'limit' must be optional"
        );
        assert!(
            !required_names.contains(&"reset_cursor"),
            "check_inbox 'reset_cursor' must be optional"
        );
        assert!(
            !required_names.contains(&"repo"),
            "check_inbox 'repo' must be optional"
        );
        assert!(
            !required_names.contains(&"session"),
            "check_inbox 'session' must be optional"
        );
        assert!(
            !required_names.contains(&"tag"),
            "check_inbox 'tag' must be optional"
        );
        assert!(
            !required_names.contains(&"thread_id"),
            "check_inbox 'thread_id' must be optional"
        );
    }

    /// `check_inbox` schema must declare cursor and scope filter properties.
    #[test]
    fn check_inbox_schema_optional_properties_present() {
        let tools = AgentBusMcpServer::tool_list();
        let tool = tools
            .iter()
            .find(|t| t.name == "check_inbox")
            .expect("check_inbox tool must exist");

        let props = tool
            .input_schema
            .get("properties")
            .and_then(|v| v.as_object())
            .expect("check_inbox schema must have 'properties'");

        assert!(
            props.contains_key("limit"),
            "check_inbox must have 'limit' property"
        );
        assert!(
            props.contains_key("reset_cursor"),
            "check_inbox must have 'reset_cursor' property"
        );
        assert!(
            props.contains_key("repo"),
            "check_inbox must have 'repo' property"
        );
        assert!(
            props.contains_key("session"),
            "check_inbox must have 'session' property"
        );
        assert!(
            props.contains_key("tag"),
            "check_inbox must have 'tag' property"
        );
        assert!(
            props.contains_key("thread_id"),
            "check_inbox must have 'thread_id' property"
        );
    }

    /// Calling `check_inbox` without the required `agent` argument returns an error.
    #[test]
    fn check_inbox_requires_agent_arg() {
        let server = AgentBusMcpServer::new(crate::settings::Settings::from_env());
        // Provide limit but omit agent.
        let args = serde_json::json!({"limit": 5});
        let result = server.call_tool_sync(
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

    /// `check_inbox` cursor key format matches the expected
    /// `bus:notify_cursor:<agent>` pattern.
    #[test]
    fn check_inbox_cursor_key_format() {
        assert_eq!(
            notification_cursor_key("claude"),
            "bus:notify_cursor:claude"
        );
        assert_eq!(notification_cursor_key("codex"), "bus:notify_cursor:codex");
    }
}
