//! MCP server handler exposing agent-bus operations as MCP tools.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::ServerHandler;

use crate::redis_bus::{
    bus_health, bus_list_messages, bus_list_presence, bus_post_message, bus_set_presence, connect,
};
use crate::settings::Settings;
use crate::validation::validate_priority;

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
        // Build minimal but valid JSON Schema objects for each tool
        let schema_for = |props: serde_json::Value,
                          required: &[&str]|
         -> Arc<serde_json::Map<String, serde_json::Value>> {
            let mut schema = serde_json::Map::new();
            schema.insert("type".to_owned(), serde_json::json!("object"));
            schema.insert("properties".to_owned(), props);
            if !required.is_empty() {
                schema.insert(
                    "required".to_owned(),
                    serde_json::Value::Array(
                        required.iter().map(|s| serde_json::json!(s)).collect(),
                    ),
                );
            }
            Arc::new(schema)
        };

        vec![
            Tool::new(
                "bus_health",
                "Check the health of the agent-bus Redis backend.",
                schema_for(serde_json::json!({}), &[]),
            ),
            Tool::new(
                "post_message",
                "Post a message to the agent coordination bus.",
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
                        "metadata":  {"type": "object"}
                    }),
                    &["sender", "recipient", "topic", "body"],
                ),
            ),
            Tool::new(
                "list_messages",
                "List recent messages from the bus, optionally filtered by recipient or sender.",
                schema_for(
                    serde_json::json!({
                        "agent":          {"type": "string"},
                        "sender":         {"type": "string"},
                        "since_minutes":  {"type": "integer", "minimum": 1, "maximum": 10080},
                        "limit":          {"type": "integer", "minimum": 1, "maximum": 500},
                        "include_broadcast": {"type": "boolean"}
                    }),
                    &[],
                ),
            ),
            Tool::new(
                "ack_message",
                "Acknowledge a message by posting an ack reply.",
                schema_for(
                    serde_json::json!({
                        "agent":      {"type": "string"},
                        "message_id": {"type": "string"},
                        "body":       {"type": "string"}
                    }),
                    &["agent", "message_id"],
                ),
            ),
            Tool::new(
                "set_presence",
                "Announce or update agent presence on the bus.",
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
                ),
            ),
            Tool::new(
                "list_presence",
                "List all active agent presence records.",
                schema_for(serde_json::json!({}), &[]),
            ),
        ]
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
        reason = "match on 6 tool names, each short — extracting further would reduce clarity"
    )]
    fn handle_tool_call_inner(
        &self,
        name: &str,
        args: &serde_json::Map<String, serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let settings = &*self.settings;

        match name {
            "bus_health" => {
                let h = bus_health(settings);
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

                validate_priority(priority)?;
                let mut conn = connect(settings)?;
                let msg = bus_post_message(
                    &mut conn,
                    settings,
                    sender,
                    recipient,
                    topic,
                    body,
                    thread_id,
                    &tags,
                    priority,
                    request_ack,
                    reply_to,
                    &metadata,
                )?;
                Ok(serde_json::to_value(&msg)?)
            }

            "list_messages" => {
                let agent = Self::get_str(args, "agent");
                let sender = Self::get_str(args, "sender");
                let since_minutes = Self::get_u64_or(args, "since_minutes", 1440);
                let limit = Self::get_usize_or(args, "limit", 50);
                let include_broadcast = Self::get_bool_or(args, "include_broadcast", true);

                let msgs = bus_list_messages(
                    settings,
                    agent,
                    sender,
                    since_minutes,
                    limit,
                    include_broadcast,
                )?;
                Ok(serde_json::to_value(&msgs)?)
            }

            "ack_message" => {
                let agent =
                    Self::get_str(args, "agent").ok_or_else(|| anyhow!("agent is required"))?;
                let message_id = Self::get_str(args, "message_id")
                    .ok_or_else(|| anyhow!("message_id is required"))?;
                let body = Self::get_str_or(args, "body", "ack");

                let meta = serde_json::json!({"ack_for": message_id});
                let mut conn = connect(settings)?;
                let msg = bus_post_message(
                    &mut conn,
                    settings,
                    agent,
                    "all",
                    "ack",
                    body,
                    None,
                    &[],
                    "normal",
                    false,
                    Some(message_id),
                    &meta,
                )?;
                Ok(serde_json::to_value(&msg)?)
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
                let presence = bus_set_presence(
                    &mut conn,
                    settings,
                    agent,
                    status,
                    session_id,
                    &capabilities,
                    ttl_seconds,
                    &metadata,
                )?;
                Ok(serde_json::to_value(&presence)?)
            }

            "list_presence" => {
                let mut conn = connect(settings)?;
                let results = bus_list_presence(&mut conn, settings)?;
                Ok(serde_json::to_value(&results)?)
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
            "Redis-backed coordination bus with PostgreSQL-backed durable history for local agents. \
             Use post_message for handoffs, list_messages to inspect inbox history, \
             set_presence to advertise availability, and list_presence to discover \
             other active agents.",
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
