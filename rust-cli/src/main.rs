//! Fast standalone CLI + MCP server for agent-bus coordination.
//!
//! Direct Redis client — no Python interpreter, no GIL, instant startup.
//! Full feature parity with the Python agent-bus-mcp server, including MCP stdio mode.
//!
//! # Example
//!
//! ```bash
//! agent-bus health --encoding compact
//! agent-bus send --from-agent claude --to-agent codex --topic test --body "hello"
//! agent-bus read --agent codex --since-minutes 5 --encoding human
//! agent-bus serve --transport stdio
//! ```

// ---------------------------------------------------------------------------
// Lint suppressions at the crate level
// ---------------------------------------------------------------------------
#![expect(
    clippy::multiple_crate_versions,
    reason = "dependency diamond between rmcp and redis — not our choice"
)]

mod models;
mod output;
mod postgres_store;
mod redis_bus;
mod settings;
mod validation;

use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post, put},
};
use clap::{Parser, Subcommand};
use mimalloc::MiMalloc;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, Content, Implementation, InitializeResult,
    ListToolsResult, PaginatedRequestParams, ServerCapabilities, Tool,
};
use rmcp::{ServerHandler, serve_server};
use serde::Deserialize;

use models::{MAX_HISTORY_MINUTES, Message, Presence, STARTUP_PRESENCE_TTL, default_priority};
use output::{Encoding, output, output_message, output_messages, output_presence};
use postgres_store::probe_postgres;
use redis_bus::{
    bus_health, bus_list_messages, bus_list_presence, bus_post_message, bus_set_presence, connect,
};
use settings::Settings;
use validation::{non_empty, parse_metadata_arg, validate_priority};

// Items re-exported into scope solely so that `use super::*` in the test
// module can access them without additional per-test imports.
#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use output::minimize_value;
#[cfg(test)]
use redis_bus::{decode_stream_entry, parse_xrange_result};
#[cfg(test)]
use settings::{env_or, env_parse, redact_url};
#[cfg(test)]
use validation::VALID_PRIORITIES;

/// Use mimalloc for notable allocation performance gains (M-MIMALLOC-APPS).
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// ---------------------------------------------------------------------------
// Section 5 – CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser)]
#[command(
    name = "agent-bus",
    version,
    about = "Redis + PostgreSQL agent coordination bus — CLI + MCP server (Rust native)",
    long_about = "Redis + PostgreSQL coordination bus for AI coding agents.\n\n\
        Provides message passing, presence tracking, and real-time event streaming\n\
        between Claude, Codex, Gemini, Copilot, and custom agents on the same machine.\n\n\
        Protocol: agent-bus v1.0 | Backend: Redis Stream + Pub/Sub + PostgreSQL history\n\
        Codec: serde_json + MessagePack + LZ4 | Runtime: Rust native (no Python)\n\n\
        Stable agent IDs: claude, codex, gemini, copilot, euler, pasteur, all",
    after_help = "ENVIRONMENT VARIABLES:\n  \
        AGENT_BUS_REDIS_URL          Redis connection URL [default: redis://localhost:6380/0]\n  \
        AGENT_BUS_DATABASE_URL       PostgreSQL connection URL [default: postgresql://postgres@localhost:5432/redis_backend]\n  \
        AGENT_BUS_STREAM_KEY         Redis Stream key [default: agent_bus:messages]\n  \
        AGENT_BUS_CHANNEL            Redis Pub/Sub channel [default: agent_bus:events]\n  \
        AGENT_BUS_PRESENCE_PREFIX    Redis key prefix for presence [default: agent_bus:presence:]\n  \
        AGENT_BUS_STREAM_MAXLEN      Max stream entries [default: 5000]\n  \
        AGENT_BUS_SERVER_HOST        HTTP bind host [default: localhost]\n  \
        AGENT_BUS_SERVICE_AGENT_ID   Agent ID for this service [default: agent-bus]\n  \
        AGENT_BUS_STARTUP_ENABLED    Announce on MCP startup [default: true]\n  \
        AGENT_BUS_STARTUP_RECIPIENT  Startup message recipient [default: all]\n  \
        AGENT_BUS_STARTUP_TOPIC      Startup message topic [default: status]\n  \
        AGENT_BUS_STARTUP_BODY       Startup message body\n  \
        RUST_LOG                     Log level filter [default: error]\n\n\
        ENCODING MODES:\n  \
        compact   Machine-optimized JSON, no whitespace (default for scripts/CI)\n  \
        json      Pretty-printed JSON with 2-space indent (debugging)\n  \
        minimal   Token-minimized: short field names, defaults stripped (~50% fewer tokens)\n  \
        human     Table format for terminal reading (read/watch only)\n\n\
        DOCUMENTATION:\n  \
        Protocol spec:    ~/.agents/AGENT_COORDINATION.md\n  \
        Canonical docs:   ~/.codex/docs/AGENT_BUS.md\n  \
        Rust dev guide:   ~/.agents/rust-development-guide.md\n  \
        Impl notes:       ~/.codex/tools/agent-bus-mcp/IMPLEMENTATION_NOTES.md\n\n\
        EXAMPLES:\n  \
        agent-bus health\n  \
        agent-bus send --from-agent claude --to-agent codex --topic status --body \"ready\"\n  \
        agent-bus read --agent claude --since-minutes 60 --encoding human\n  \
        agent-bus watch --agent claude --history 10 --encoding human\n  \
        agent-bus ack --agent claude --message-id <UUID>\n  \
        agent-bus presence --agent claude --capability mcp --capability rust\n  \
        agent-bus serve --transport stdio  # MCP server mode for mcp.json"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check Redis bus health and report runtime metadata.
    #[command(long_about = "Ping Redis and, when configured, PostgreSQL.\n\n\
        Returns: ok, protocol_version, redis_url, database_url, database_ok,\n\
        database_error, storage_ready, runtime, codec.\n\
        Use --encoding compact for CI/dashboards, --encoding json for debugging.")]
    Health {
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Post a message to the coordination bus.
    #[command(
        long_about = "Post a message to the Redis Stream and Pub/Sub channel.\n\n\
        Messages are stored in the stream (up to AGENT_BUS_STREAM_MAXLEN entries)\n\
        and broadcast to all pub/sub subscribers. Use --thread-id to group related\n\
        messages. Use --request-ack when you need confirmation from the recipient.\n\n\
        Priority levels: low, normal (default), high, urgent"
    )]
    Send {
        #[arg(long, help = "Sender agent ID (e.g. claude, codex)")]
        from_agent: String,
        #[arg(long, help = "Recipient agent ID or 'all' for broadcast")]
        to_agent: String,
        #[arg(
            long,
            help = "Message topic (e.g. review-findings, file-ownership, status)"
        )]
        topic: String,
        #[arg(long, help = "Message body text")]
        body: String,
        #[arg(long, help = "Optional thread ID to group related messages")]
        thread_id: Option<String>,
        #[arg(long, action = clap::ArgAction::Append, help = "Add a tag (repeatable)")]
        tag: Vec<String>,
        #[arg(
            long,
            default_value = "normal",
            help = "Priority: low|normal|high|urgent"
        )]
        priority: String,
        #[arg(
            long,
            default_value_t = false,
            help = "Request acknowledgement from recipient"
        )]
        request_ack: bool,
        #[arg(long, help = "Reply-to agent ID [default: sender]")]
        reply_to: Option<String>,
        #[arg(long, help = "JSON metadata object (e.g. '{\"key\":\"value\"}')")]
        metadata: Option<String>,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Read recent messages from the bus.
    #[command(long_about = "Query the Redis Stream for recent messages.\n\n\
        Messages are returned in chronological order (oldest first).\n\
        Use --agent to filter by recipient, --from-agent to filter by sender.\n\
        Broadcast messages (to='all') are included by default.")]
    Read {
        #[arg(long, help = "Filter by recipient agent ID")]
        agent: Option<String>,
        #[arg(long, help = "Filter by sender agent ID")]
        from_agent: Option<String>,
        #[arg(
            long,
            default_value_t = 1440,
            help = "Time window in minutes [1-10080]"
        )]
        since_minutes: u64,
        #[arg(long, default_value_t = 50, help = "Max messages to return [1-500]")]
        limit: usize,
        #[arg(
            long,
            default_value_t = false,
            help = "Exclude broadcast (to='all') messages"
        )]
        exclude_broadcast: bool,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Watch bus traffic in real-time via Redis Pub/Sub.
    #[command(
        long_about = "Subscribe to the bus Pub/Sub channel and stream events.\n\n\
        Optionally loads --history N prior messages before switching to live mode.\n\
        Runs until interrupted (Ctrl+C). Events include both messages and presence.\n\n\
        Human format: [timestamp] from -> to | topic | priority | body\n\
        Presence:     [timestamp] presence agent=status session=id"
    )]
    Watch {
        #[arg(long, help = "Agent ID to filter events for")]
        agent: String,
        #[arg(
            long,
            default_value_t = 0,
            help = "Load N historical messages before streaming"
        )]
        history: u64,
        #[arg(long, default_value_t = false, help = "Exclude broadcast messages")]
        exclude_broadcast: bool,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Acknowledge a message by posting a reply.
    #[command(long_about = "Post an acknowledgement for a specific message.\n\n\
        Creates a new message with topic='ack', to='all', reply_to=message_id,\n\
        and metadata.ack_for=message_id. The original sender can filter for acks\n\
        by checking reply_to.")]
    Ack {
        #[arg(long, help = "Your agent ID")]
        agent: String,
        #[arg(long, help = "UUID of the message to acknowledge")]
        message_id: String,
        #[arg(long, default_value = "ack", help = "Ack body text")]
        body: String,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Set or update agent presence with TTL expiry.
    #[command(
        long_about = "Register agent availability in Redis with automatic TTL expiry.\n\n\
        Presence records expire after --ttl-seconds (default 180s = 3 minutes).\n\
        Use --capability to advertise supported features (e.g. mcp, redis, rust).\n\
        Presence changes are also published to the Pub/Sub channel."
    )]
    Presence {
        #[arg(long, help = "Agent ID to set presence for")]
        agent: String,
        #[arg(long, default_value = "online", help = "Status: online|offline|busy")]
        status: String,
        #[arg(long, help = "Session UUID [default: auto-generated]")]
        session_id: Option<String>,
        #[arg(long, action = clap::ArgAction::Append, help = "Capability tag (repeatable)")]
        capability: Vec<String>,
        #[arg(long, default_value_t = 180, help = "TTL in seconds [1-86400]")]
        ttl_seconds: u64,
        #[arg(long, help = "JSON metadata object")]
        metadata: Option<String>,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// List all active agent presence records.
    #[command(
        long_about = "Scan Redis for all presence keys and return sorted by agent name.\n\
        Only shows agents whose TTL has not expired."
    )]
    PresenceList {
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Run as an MCP server (stdio transport) or HTTP REST server.
    #[command(
        long_about = "Start a Model Context Protocol (MCP) server on stdio, or an HTTP REST server.\n\n\
        MCP mode (default):\n\
        Exposes 6 tools to LLM agents: bus_health, post_message, list_messages,\n\
        ack_message, set_presence, list_presence.\n\
        Register in mcp.json / config.toml / settings.json:\n\
        {\"command\": \"agent-bus\", \"args\": [\"serve\", \"--transport\", \"stdio\"]}\n\n\
        HTTP mode (--transport http):\n\
        Starts an axum HTTP server on localhost:PORT (default 8400).\n\
        Routes: GET /health, POST /messages, GET /messages, POST /messages/:id/ack,\n\
        PUT /presence/:agent, GET /presence\n\n\
        On startup, announces presence and posts a startup message if\n\
        AGENT_BUS_STARTUP_ENABLED=true (default)."
    )]
    Serve {
        #[arg(long, default_value = "stdio", help = "Transport protocol: stdio|http")]
        transport: String,
        #[arg(
            long,
            default_value_t = 8400,
            help = "HTTP server port (only used when --transport http)"
        )]
        port: u16,
    },
}

// ---------------------------------------------------------------------------
// Section 7 – Command implementations
// ---------------------------------------------------------------------------

fn cmd_health(settings: &Settings, encoding: &Encoding) {
    let health = bus_health(settings);
    output(&health, encoding);
}

fn cmd_send(settings: &Settings, args: &SendArgs) -> Result<()> {
    validate_priority(args.priority)?;
    let from = non_empty(args.from_agent, "--from-agent")?;
    let to = non_empty(args.to_agent, "--to-agent")?;
    let topic = non_empty(args.topic, "--topic")?;
    let body = non_empty(args.body, "--body")?;
    let meta = parse_metadata_arg(args.metadata)?;

    let mut conn = connect(settings)?;
    let msg = bus_post_message(
        &mut conn,
        settings,
        from,
        to,
        topic,
        body,
        args.thread_id.as_deref(),
        args.tags,
        args.priority,
        args.request_ack,
        args.reply_to.as_deref(),
        &meta,
    )?;
    output(&msg, args.encoding);
    Ok(())
}

fn cmd_read(settings: &Settings, args: &ReadArgs) -> Result<()> {
    let msgs = bus_list_messages(
        settings,
        args.agent.as_deref(),
        args.from_agent.as_deref(),
        args.since_minutes,
        args.limit,
        !args.exclude_broadcast,
    )?;
    output_messages(&msgs, args.encoding);
    Ok(())
}

fn cmd_watch(
    settings: &Settings,
    agent: &str,
    history: u64,
    exclude_broadcast: bool,
    encoding: &Encoding,
) -> Result<()> {
    // Load history first (chronological order)
    if history > 0 {
        let msgs = bus_list_messages(
            settings,
            Some(agent),
            None,
            MAX_HISTORY_MINUTES,
            usize::try_from(history).unwrap_or(usize::MAX),
            !exclude_broadcast,
        )?;
        for msg in &msgs {
            output_message(msg, encoding);
        }
    }

    // Subscribe to pub/sub
    let client = redis::Client::open(settings.redis_url.as_str())
        .context("Redis client for watch failed")?;
    let mut conn = client
        .get_connection()
        .context("Redis watch connection failed")?;
    let mut pubsub = conn.as_pubsub();
    pubsub
        .subscribe(&settings.channel_key)
        .context("SUBSCRIBE failed")?;

    let include_broadcast = !exclude_broadcast;
    loop {
        let msg = pubsub.get_message().context("pub/sub receive error")?;
        let payload: String = msg.get_payload().context("pub/sub payload decode")?;
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&payload) else {
            continue;
        };
        match event.get("event").and_then(|e| e.as_str()) {
            Some("message") => {
                let Some(msg_val) = event.get("message") else {
                    continue;
                };
                let Ok(bus_msg) = serde_json::from_value::<Message>(msg_val.clone()) else {
                    continue;
                };
                let to_matches = bus_msg.to == agent;
                let broadcast_matches = include_broadcast && bus_msg.to == "all";
                if to_matches || broadcast_matches {
                    output_message(&bus_msg, encoding);
                }
            }
            Some("presence") => {
                let Some(p_val) = event.get("presence") else {
                    continue;
                };
                let Ok(presence) = serde_json::from_value::<Presence>(p_val.clone()) else {
                    continue;
                };
                output_presence(&presence, encoding);
            }
            _ => {}
        }
    }
}

fn cmd_ack(
    settings: &Settings,
    agent: &str,
    message_id: &str,
    body: &str,
    encoding: &Encoding,
) -> Result<()> {
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
        Some(message_id), // reply_to = the message being acked
        &meta,
    )?;
    output(&msg, encoding);
    Ok(())
}

fn cmd_presence(settings: &Settings, args: &PresenceArgs) -> Result<()> {
    let agent = non_empty(args.agent, "--agent")?;
    let meta = parse_metadata_arg(args.metadata)?;
    let mut conn = connect(settings)?;
    let presence = bus_set_presence(
        &mut conn,
        settings,
        agent,
        args.status,
        args.session_id.as_deref(),
        args.capabilities,
        args.ttl_seconds,
        &meta,
    )?;
    output_presence(&presence, args.encoding);
    Ok(())
}

fn cmd_presence_list(settings: &Settings, encoding: &Encoding) -> Result<()> {
    let mut conn = connect(settings)?;
    let results = bus_list_presence(&mut conn, settings)?;
    if matches!(encoding, Encoding::Human) {
        for p in &results {
            output_presence(p, encoding);
        }
    } else {
        output(&results, encoding);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Argument structs (avoid cloning in Cmd match)
// ---------------------------------------------------------------------------

struct SendArgs<'a> {
    from_agent: &'a str,
    to_agent: &'a str,
    topic: &'a str,
    body: &'a str,
    thread_id: &'a Option<String>,
    tags: &'a [String],
    priority: &'a str,
    request_ack: bool,
    reply_to: &'a Option<String>,
    metadata: Option<&'a str>,
    encoding: &'a Encoding,
}

struct ReadArgs<'a> {
    agent: &'a Option<String>,
    from_agent: &'a Option<String>,
    since_minutes: u64,
    limit: usize,
    exclude_broadcast: bool,
    encoding: &'a Encoding,
}

struct PresenceArgs<'a> {
    agent: &'a str,
    status: &'a str,
    session_id: &'a Option<String>,
    capabilities: &'a [String],
    ttl_seconds: u64,
    metadata: Option<&'a str>,
    encoding: &'a Encoding,
}

// ---------------------------------------------------------------------------
// Section 8 – MCP Server
// ---------------------------------------------------------------------------

/// MCP server handler that exposes agent-bus operations as MCP tools.
///
/// Settings are immutable after construction, so no `Mutex` is needed.
struct AgentBusMcpServer {
    settings: Arc<Settings>,
}

impl AgentBusMcpServer {
    fn new(settings: Settings) -> Self {
        Self {
            settings: Arc::new(settings),
        }
    }

    fn tool_list() -> Vec<Tool> {
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

// ---------------------------------------------------------------------------
// Section 8b – HTTP REST server (axum)
// ---------------------------------------------------------------------------

/// Shared state injected into every axum handler.
type AppState = Arc<Settings>;

/// Map an `anyhow::Error` to an HTTP 500 response with a JSON body.
#[expect(
    clippy::needless_pass_by_value,
    reason = "used as map_err(internal_error) — fn pointer requires by-value"
)]
fn internal_error(e: anyhow::Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": format!("{e:#}")})),
    )
}

/// Map a bad-input string to an HTTP 400 response with a JSON body.
fn bad_request(msg: impl Into<String>) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": msg.into()})),
    )
}

// --- GET /health -----------------------------------------------------------

async fn http_health_handler(State(settings): State<AppState>) -> impl IntoResponse {
    let h = bus_health(&settings);
    Json(serde_json::to_value(&h).unwrap_or_default())
}

// --- POST /messages --------------------------------------------------------

/// Request body for POST /messages.
#[derive(Debug, Deserialize)]
struct HttpSendRequest {
    sender: String,
    recipient: String,
    topic: String,
    body: String,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default = "default_priority")]
    priority: String,
    #[serde(default)]
    request_ack: bool,
    #[serde(default)]
    reply_to: Option<String>,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

async fn http_send_handler(
    State(settings): State<AppState>,
    Json(req): Json<HttpSendRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // Validate required fields.
    let sender = req.sender.trim().to_owned();
    let recipient = req.recipient.trim().to_owned();
    let topic = req.topic.trim().to_owned();
    let body_text = req.body.trim().to_owned();

    if sender.is_empty() {
        return Err(bad_request("sender must not be empty"));
    }
    if recipient.is_empty() {
        return Err(bad_request("recipient must not be empty"));
    }
    if topic.is_empty() {
        return Err(bad_request("topic must not be empty"));
    }
    if body_text.is_empty() {
        return Err(bad_request("body must not be empty"));
    }
    validate_priority(&req.priority).map_err(|e| bad_request(format!("{e:#}")))?;

    let metadata = req
        .metadata
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    let mut conn = connect(&settings).map_err(internal_error)?;
    let msg = bus_post_message(
        &mut conn,
        &settings,
        &sender,
        &recipient,
        &topic,
        &body_text,
        req.thread_id.as_deref(),
        &req.tags,
        &req.priority,
        req.request_ack,
        req.reply_to.as_deref(),
        &metadata,
    )
    .map_err(internal_error)?;

    Ok((
        StatusCode::OK,
        Json(serde_json::to_value(&msg).unwrap_or_default()),
    ))
}

// --- GET /messages ---------------------------------------------------------

/// Query parameters for GET /messages.
#[derive(Debug, Deserialize)]
struct HttpReadQuery {
    agent: Option<String>,
    from: Option<String>,
    #[serde(default = "default_since_minutes")]
    since: u64,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_true")]
    broadcast: bool,
}

fn default_since_minutes() -> u64 {
    60
}

fn default_limit() -> usize {
    50
}

fn default_true() -> bool {
    true
}

async fn http_read_handler(
    State(settings): State<AppState>,
    Query(params): Query<HttpReadQuery>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let since = params.since.min(MAX_HISTORY_MINUTES);
    let limit = params.limit.clamp(1, 500);

    let msgs = bus_list_messages(
        &settings,
        params.agent.as_deref(),
        params.from.as_deref(),
        since,
        limit,
        params.broadcast,
    )
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&msgs).unwrap_or_default()))
}

// --- POST /messages/:id/ack ------------------------------------------------

/// Request body for POST /messages/:id/ack.
#[derive(Debug, Deserialize)]
struct HttpAckRequest {
    agent: String,
    #[serde(default = "default_ack_body")]
    body: String,
}

fn default_ack_body() -> String {
    "ack".to_owned()
}

async fn http_ack_handler(
    State(settings): State<AppState>,
    Path(message_id): Path<String>,
    Json(req): Json<HttpAckRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let agent = req.agent.trim().to_owned();
    if agent.is_empty() {
        return Err(bad_request("agent must not be empty"));
    }
    let message_id = message_id.trim().to_owned();
    if message_id.is_empty() {
        return Err(bad_request("message id must not be empty"));
    }

    let meta = serde_json::json!({"ack_for": &message_id});
    let mut conn = connect(&settings).map_err(internal_error)?;
    let msg = bus_post_message(
        &mut conn,
        &settings,
        &agent,
        "all",
        "ack",
        &req.body,
        None,
        &[],
        "normal",
        false,
        Some(&message_id),
        &meta,
    )
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&msg).unwrap_or_default()))
}

// --- PUT /presence/:agent --------------------------------------------------

/// Request body for PUT /presence/:agent.
#[derive(Debug, Deserialize)]
struct HttpPresenceRequest {
    #[serde(default = "default_status")]
    status: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    capabilities: Vec<String>,
    #[serde(default = "default_ttl")]
    ttl_seconds: u64,
    #[serde(default)]
    metadata: Option<serde_json::Value>,
}

fn default_status() -> String {
    "online".to_owned()
}

fn default_ttl() -> u64 {
    180
}

async fn http_presence_set_handler(
    State(settings): State<AppState>,
    Path(agent): Path<String>,
    Json(req): Json<HttpPresenceRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let agent = agent.trim().to_owned();
    if agent.is_empty() {
        return Err(bad_request("agent must not be empty"));
    }

    let ttl = req.ttl_seconds.clamp(1, 86400);
    let metadata = req
        .metadata
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));

    let mut conn = connect(&settings).map_err(internal_error)?;
    let presence = bus_set_presence(
        &mut conn,
        &settings,
        &agent,
        &req.status,
        req.session_id.as_deref(),
        &req.capabilities,
        ttl,
        &metadata,
    )
    .map_err(internal_error)?;

    Ok(Json(serde_json::to_value(&presence).unwrap_or_default()))
}

// --- GET /presence ---------------------------------------------------------

async fn http_presence_list_handler(
    State(settings): State<AppState>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let mut conn = connect(&settings).map_err(internal_error)?;
    let results = bus_list_presence(&mut conn, &settings).map_err(internal_error)?;
    Ok(Json(serde_json::to_value(&results).unwrap_or_default()))
}

// --- Server bootstrap ------------------------------------------------------

async fn start_http_server(settings: Settings, port: u16) -> Result<()> {
    let bind_host = settings.server_host.clone();
    let state: AppState = Arc::new(settings);
    let app = Router::new()
        .route("/health", get(http_health_handler))
        .route("/messages", post(http_send_handler).get(http_read_handler))
        .route("/messages/{id}/ack", post(http_ack_handler))
        .route("/presence/{agent}", put(http_presence_set_handler))
        .route("/presence", get(http_presence_list_handler))
        .with_state(state);

    let addr = format!("{bind_host}:{port}");
    tracing::info!("HTTP server listening on {addr}");
    eprintln!("agent-bus HTTP server listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .context("failed to bind HTTP server")?;
    axum::serve(listener, app)
        .await
        .context("HTTP server error")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Section 9 – Startup announcement helper
// ---------------------------------------------------------------------------

fn maybe_announce_startup(settings: &Settings) {
    if !settings.startup_enabled {
        return;
    }
    let Ok(mut conn) = connect(settings) else {
        return; // silently skip if Redis not up yet
    };
    let meta = serde_json::json!({"service": "agent-bus", "startup": true});
    let mut caps = vec!["mcp".to_owned(), "redis".to_owned()];
    if probe_postgres(settings).0 == Some(true) {
        caps.push("postgres".to_owned());
    }
    let _ = bus_set_presence(
        &mut conn,
        settings,
        &settings.service_agent_id,
        "online",
        None,
        &caps,
        STARTUP_PRESENCE_TTL,
        &meta,
    );
    let _ = bus_post_message(
        &mut conn,
        settings,
        &settings.service_agent_id,
        &settings.startup_recipient,
        &settings.startup_topic,
        &settings.startup_body,
        None,
        &[
            "startup".to_owned(),
            "system".to_owned(),
            "health".to_owned(),
        ],
        "normal",
        false,
        None,
        &meta,
    );
}

// ---------------------------------------------------------------------------
// Section 10 – main
// ---------------------------------------------------------------------------

#[expect(
    clippy::too_many_lines,
    reason = "main command dispatch — extracting further would obscure flow"
)]
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let settings = Settings::from_env();
    let cli = Cli::parse();

    match cli.command {
        Cmd::Health { ref encoding } => {
            cmd_health(&settings, encoding);
        }

        Cmd::Send {
            ref from_agent,
            ref to_agent,
            ref topic,
            ref body,
            ref thread_id,
            ref tag,
            ref priority,
            request_ack,
            ref reply_to,
            ref metadata,
            ref encoding,
        } => {
            cmd_send(
                &settings,
                &SendArgs {
                    from_agent,
                    to_agent,
                    topic,
                    body,
                    thread_id,
                    tags: tag,
                    priority,
                    request_ack,
                    reply_to,
                    metadata: metadata.as_deref(),
                    encoding,
                },
            )?;
        }

        Cmd::Read {
            ref agent,
            ref from_agent,
            since_minutes,
            limit,
            exclude_broadcast,
            ref encoding,
        } => {
            cmd_read(
                &settings,
                &ReadArgs {
                    agent,
                    from_agent,
                    since_minutes,
                    limit,
                    exclude_broadcast,
                    encoding,
                },
            )?;
        }

        Cmd::Watch {
            ref agent,
            history,
            exclude_broadcast,
            ref encoding,
        } => {
            cmd_watch(&settings, agent, history, exclude_broadcast, encoding)?;
        }

        Cmd::Ack {
            ref agent,
            ref message_id,
            ref body,
            ref encoding,
        } => {
            cmd_ack(&settings, agent, message_id, body, encoding)?;
        }

        Cmd::Presence {
            ref agent,
            ref status,
            ref session_id,
            ref capability,
            ttl_seconds,
            ref metadata,
            ref encoding,
        } => {
            cmd_presence(
                &settings,
                &PresenceArgs {
                    agent,
                    status,
                    session_id,
                    capabilities: capability,
                    ttl_seconds,
                    metadata: metadata.as_deref(),
                    encoding,
                },
            )?;
        }

        Cmd::PresenceList { ref encoding } => {
            cmd_presence_list(&settings, encoding)?;
        }

        Cmd::Serve {
            ref transport,
            port,
        } => {
            if transport == "http" {
                maybe_announce_startup(&settings);
                start_http_server(settings, port).await?;
            } else {
                // Default: MCP stdio transport.
                // Start the MCP server FIRST so it can respond to initialize
                // immediately, then announce startup in the background.
                let announce_settings = settings.clone();
                let server = AgentBusMcpServer::new(settings);
                let mcp_transport = (tokio::io::stdin(), tokio::io::stdout());
                let service = serve_server(server, mcp_transport)
                    .await
                    .context("MCP server init failed")?;
                // Announce after MCP handshake completes (non-blocking)
                tokio::spawn(async move {
                    maybe_announce_startup(&announce_settings);
                });
                service.waiting().await.context("MCP server loop error")?;
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Section 11 – Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- validate_priority --------------------------------------------------

    #[test]
    fn validate_priority_accepts_all_valid_values() {
        for &p in VALID_PRIORITIES {
            assert!(validate_priority(p).is_ok(), "expected '{p}' to be valid");
        }
    }

    #[test]
    fn validate_priority_rejects_unknown() {
        assert!(validate_priority("critical").is_err());
        assert!(validate_priority("").is_err());
        assert!(validate_priority("NORMAL").is_err());
    }

    // -- non_empty ----------------------------------------------------------

    #[test]
    fn non_empty_returns_trimmed_value() {
        assert_eq!(non_empty("  hello  ", "field").unwrap(), "hello");
    }

    #[test]
    fn non_empty_rejects_blank() {
        assert!(non_empty("", "field").is_err());
        assert!(non_empty("   ", "field").is_err());
    }

    // -- parse_metadata_arg -------------------------------------------------

    #[test]
    fn parse_metadata_arg_none_returns_empty_object() {
        let val = parse_metadata_arg(None).unwrap();
        assert!(val.is_object());
        assert!(val.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_metadata_arg_valid_json() {
        let val = parse_metadata_arg(Some(r#"{"key":"value"}"#)).unwrap();
        assert_eq!(val["key"], "value");
    }

    #[test]
    fn parse_metadata_arg_invalid_json() {
        assert!(parse_metadata_arg(Some("not json")).is_err());
    }

    // -- default_priority ---------------------------------------------------

    #[test]
    fn default_priority_is_normal() {
        assert_eq!(default_priority(), "normal");
    }

    // -- minimize_value -----------------------------------------------------

    #[test]
    fn minimize_strips_defaults_and_shortens_keys() {
        let input = serde_json::json!({
            "timestamp_utc": "2026-01-01T00:00:00Z",
            "from": "claude",
            "to": "codex",
            "topic": "test",
            "body": "hello",
            "protocol_version": "1.0",
            "stream_id": "123-0",
            "tags": [],
            "metadata": {},
            "thread_id": null,
            "request_ack": false,
            "priority": "normal"
        });
        let minimized = minimize_value(&input);
        let obj = minimized.as_object().unwrap();

        // Stripped fields should be absent
        assert!(!obj.contains_key("protocol_version"));
        assert!(!obj.contains_key("stream_id"));
        assert!(!obj.contains_key("tags"));
        assert!(!obj.contains_key("tg"));
        assert!(!obj.contains_key("metadata"));
        assert!(!obj.contains_key("m"));
        assert!(!obj.contains_key("thread_id"));
        assert!(!obj.contains_key("tid"));
        assert!(!obj.contains_key("request_ack"));
        assert!(!obj.contains_key("ack"));
        assert!(!obj.contains_key("priority"));
        assert!(!obj.contains_key("p"));

        // Shortened keys should be present
        assert_eq!(obj["ts"], "2026-01-01T00:00:00Z");
        assert_eq!(obj["f"], "claude");
        assert_eq!(obj["t"], "codex");
        assert_eq!(obj["tp"], "test");
        assert_eq!(obj["b"], "hello");
    }

    #[test]
    fn minimize_preserves_non_default_values() {
        let input = serde_json::json!({
            "priority": "high",
            "request_ack": true,
            "tags": ["important"],
            "metadata": {"key": "val"},
            "thread_id": "abc-123"
        });
        let minimized = minimize_value(&input);
        let obj = minimized.as_object().unwrap();

        assert_eq!(obj["p"], "high");
        assert_eq!(obj["ack"], true);
        assert_eq!(obj["tg"], serde_json::json!(["important"]));
        assert_eq!(obj["m"], serde_json::json!({"key": "val"}));
        assert_eq!(obj["tid"], "abc-123");
    }

    // -- env helpers --------------------------------------------------------

    #[test]
    fn env_or_returns_default_for_missing_var() {
        let result = env_or("__AGENT_BUS_TEST_NONEXISTENT_KEY__", "fallback");
        assert_eq!(result, "fallback");
    }

    #[test]
    fn env_parse_returns_default_for_missing_var() {
        let result: u64 = env_parse("__AGENT_BUS_TEST_NONEXISTENT_KEY__", 42);
        assert_eq!(result, 42);
    }

    // -- Settings -----------------------------------------------------------

    #[test]
    fn settings_from_env_has_sane_defaults() {
        let s = Settings::from_env();
        assert!(s.redis_url.starts_with("redis://"));
        assert!(!s.stream_key.is_empty());
        assert!(!s.channel_key.is_empty());
        assert!(s.stream_maxlen > 0);
    }

    // -- decode_stream_entry ------------------------------------------------

    #[test]
    fn decode_stream_entry_handles_empty_fields() {
        let fields: HashMap<String, redis::Value> = HashMap::new();
        let msg = decode_stream_entry(&fields);
        assert!(msg.id.is_empty());
        assert!(msg.from.is_empty());
        assert_eq!(msg.priority, "normal");
        assert!(!msg.request_ack);
    }

    #[test]
    fn decode_stream_entry_reads_bulk_string_fields() {
        let mut fields: HashMap<String, redis::Value> = HashMap::new();
        fields.insert(
            "id".to_owned(),
            redis::Value::BulkString(b"msg-123".to_vec()),
        );
        fields.insert(
            "from".to_owned(),
            redis::Value::BulkString(b"claude".to_vec()),
        );
        fields.insert("to".to_owned(), redis::Value::BulkString(b"codex".to_vec()));
        fields.insert(
            "topic".to_owned(),
            redis::Value::BulkString(b"test".to_vec()),
        );
        fields.insert(
            "body".to_owned(),
            redis::Value::BulkString(b"hello".to_vec()),
        );
        fields.insert(
            "priority".to_owned(),
            redis::Value::BulkString(b"high".to_vec()),
        );
        fields.insert(
            "request_ack".to_owned(),
            redis::Value::BulkString(b"true".to_vec()),
        );
        fields.insert(
            "thread_id".to_owned(),
            redis::Value::BulkString(b"tid-1".to_vec()),
        );
        fields.insert(
            "reply_to".to_owned(),
            redis::Value::BulkString(b"claude".to_vec()),
        );

        let msg = decode_stream_entry(&fields);
        assert_eq!(msg.id, "msg-123");
        assert_eq!(msg.from, "claude");
        assert_eq!(msg.to, "codex");
        assert_eq!(msg.topic, "test");
        assert_eq!(msg.body, "hello");
        assert_eq!(msg.priority, "high");
        assert!(msg.request_ack);
        assert_eq!(msg.thread_id, Some("tid-1".to_owned()));
        assert_eq!(msg.reply_to, Some("claude".to_owned()));
    }

    #[test]
    fn decode_stream_entry_thread_id_none_is_mapped() {
        let mut fields: HashMap<String, redis::Value> = HashMap::new();
        fields.insert(
            "thread_id".to_owned(),
            redis::Value::BulkString(b"None".to_vec()),
        );
        let msg = decode_stream_entry(&fields);
        assert_eq!(msg.thread_id, None);
    }

    // -- parse_xrange_result ------------------------------------------------

    #[test]
    fn parse_xrange_result_handles_empty() {
        let raw: Vec<redis::Value> = vec![];
        let result = parse_xrange_result(&raw);
        assert!(result.is_empty());
    }

    #[test]
    fn parse_xrange_result_parses_single_entry() {
        let entry = redis::Value::Array(vec![
            redis::Value::BulkString(b"1234-0".to_vec()),
            redis::Value::Array(vec![
                redis::Value::BulkString(b"id".to_vec()),
                redis::Value::BulkString(b"msg-1".to_vec()),
                redis::Value::BulkString(b"from".to_vec()),
                redis::Value::BulkString(b"claude".to_vec()),
            ]),
        ]);
        let raw = vec![entry];
        let result = parse_xrange_result(&raw);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, "1234-0");
        assert!(result[0].1.contains_key("id"));
        assert!(result[0].1.contains_key("from"));
    }

    // -- MCP tool list ------------------------------------------------------

    #[test]
    fn tool_list_has_expected_count() {
        let tools = AgentBusMcpServer::tool_list();
        assert_eq!(tools.len(), 6, "expected 6 MCP tools");
    }

    #[test]
    fn tool_list_names_are_correct() {
        let tools = AgentBusMcpServer::tool_list();
        let names: Vec<String> = tools.iter().map(|t| t.name.to_string()).collect();
        assert!(names.iter().any(|n| n == "bus_health"));
        assert!(names.iter().any(|n| n == "post_message"));
        assert!(names.iter().any(|n| n == "list_messages"));
        assert!(names.iter().any(|n| n == "ack_message"));
        assert!(names.iter().any(|n| n == "set_presence"));
        assert!(names.iter().any(|n| n == "list_presence"));
    }

    // -- Health struct ------------------------------------------------------

    #[test]
    fn health_codec_field_is_accurate() {
        let s = Settings::from_env();
        let h = bus_health(&s);
        assert_eq!(h.codec, "serde_json");
        assert_eq!(h.runtime, "rust-native");
    }

    #[test]
    fn redact_url_hides_credentials() {
        assert_eq!(
            redact_url("postgresql://postgres:secret@localhost:5432/redis_backend"),
            "postgresql://***:***@localhost:5432/redis_backend"
        );
        assert_eq!(
            redact_url("redis://default@localhost:6380/0"),
            "redis://***@localhost:6380/0"
        );
    }

    #[test]
    fn redact_url_leaves_plain_urls_unchanged() {
        assert_eq!(
            redact_url("redis://localhost:6380/0"),
            "redis://localhost:6380/0"
        );
    }

    // -- Constants ----------------------------------------------------------

    #[test]
    fn max_history_minutes_is_one_week() {
        assert_eq!(MAX_HISTORY_MINUTES, 7 * 24 * 60);
    }
}
