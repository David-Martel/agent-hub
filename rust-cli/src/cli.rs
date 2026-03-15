//! Clap CLI parser and subcommand definitions.

use clap::{Parser, Subcommand};

use crate::output::Encoding;

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
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
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
