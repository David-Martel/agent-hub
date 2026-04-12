//! MCP server handler exposing agent-bus operations as MCP tools.
//!
//! This module is a thin transport adapter: it converts between `rmcp` types
//! and the transport-agnostic [`agent_bus_core::mcp_dispatch`] layer that
//! holds the actual tool definitions and dispatch logic.

use std::sync::Arc;

use anyhow::Result;
use rmcp::ServerHandler;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};

use agent_bus_core::settings::Settings;
use agent_bus_core::mcp_dispatch::{McpToolDispatch, ToolDefinition, tool_definitions};

/// Convert a [`ToolDefinition`] into an `rmcp::model::Tool`.
fn to_rmcp_tool(def: ToolDefinition) -> Tool {
    // rmcp::model::Tool::new expects owned/static strings and an Arc<Map<String, Value>>.
    // Our ToolDefinition stores the schema as a plain serde_json::Value (object).
    let schema_map = match def.schema {
        serde_json::Value::Object(map) => Arc::new(map),
        _ => Arc::new(serde_json::Map::new()),
    };
    Tool::new(def.name, def.description, schema_map)
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
        tool_definitions().into_iter().map(to_rmcp_tool).collect()
    }

    fn json_to_text(value: &serde_json::Value) -> Content {
        Content::text(serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned()))
    }

    fn err_content<E: std::fmt::Display>(e: &E) -> CallToolResult {
        CallToolResult::error(vec![Content::text(format!("Error: {e:#}"))])
    }

    fn ok_content(value: &serde_json::Value) -> CallToolResult {
        CallToolResult::success(vec![Self::json_to_text(value)])
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

        let dispatch = McpToolDispatch::new(&self.settings);
        match dispatch.dispatch_tool(&request.name, args_map) {
            Ok(ref value) => Ok(Self::ok_content(value)),
            Err(e) => Ok(Self::err_content(&e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bus_core::mcp_dispatch::tool_definitions;
    use agent_bus_core::redis_bus::notification_cursor_key;

    /// Helper: create a dispatch instance for tests that do not need Redis/PG.
    fn test_dispatch() -> McpToolDispatch<'static> {
        // Leak a Settings to get a 'static reference — acceptable in test code.
        let settings = Box::leak(Box::new(agent_bus_core::settings::Settings::from_env()));
        McpToolDispatch::new(settings)
    }

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
        let dispatch = test_dispatch();
        let result = dispatch.dispatch_tool("negotiate", &serde_json::Map::new());
        let val = result.expect("negotiate tool failed");
        assert_eq!(val["protocol_version"], agent_bus_core::models::PROTOCOL_VERSION);
        assert!(val["transports"].as_array().is_some());
        assert!(val["schemas"].as_array().is_some());
    }

    #[test]
    fn call_tool_sync_unknown_tool_returns_error() {
        let dispatch = test_dispatch();
        let result = dispatch.dispatch_tool("nonexistent_tool", &serde_json::Map::new());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown tool"));
    }

    #[test]
    fn negotiate_lists_channel_features() {
        let dispatch = test_dispatch();
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
        let dispatch = test_dispatch();
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
        let dispatch = test_dispatch();
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
        let dispatch = test_dispatch();

        // Missing agent
        let args1 = serde_json::json!({"resource": "src/main.rs"});
        let r1 = dispatch.dispatch_tool("claim_resource", args1.as_object().unwrap());
        assert!(r1.is_err());

        // Missing resource
        let args2 = serde_json::json!({"agent": "claude"});
        let r2 = dispatch.dispatch_tool("claim_resource", args2.as_object().unwrap());
        assert!(r2.is_err());
    }

    // ------------------------------------------------------------------
    // Schema inspection -- no Redis/PG required
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
    /// an empty array -- consistent with how `schema_for` works when `required` is `&[]`.
    #[test]
    fn schema_for_empty_required_omits_required_key() {
        // The core schemas::schema_for(props, &[]) does not insert "required" at all.
        // After conversion to rmcp Tool, the input_schema (Arc<Map>) preserves this.
        use agent_bus_core::mcp_dispatch::schemas;

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
    // check_inbox tool -- schema and dispatch tests (no Redis required)
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
        let dispatch = test_dispatch();
        // Provide limit but omit agent.
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
