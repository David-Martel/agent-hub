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
        AGENT_BUS_STREAM_MAXLEN      Max stream entries [default: 100000]\n  \
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
        #[arg(long, help = "Validate body against schema: finding|status|benchmark")]
        schema: Option<String>,
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

    /// Prune old messages and presence events from `PostgreSQL`.
    #[command(
        long_about = "Delete messages and presence events older than N days from PostgreSQL.\n\
        Useful for managing storage growth in long-running deployments."
    )]
    Prune {
        #[arg(long, default_value_t = 30, help = "Delete records older than N days")]
        older_than_days: u64,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Export messages as NDJSON (one JSON object per line).
    #[command(long_about = "Export messages to stdout as newline-delimited JSON.\n\
        Useful for backup, recovery, and analysis workflows.")]
    Export {
        #[arg(long, help = "Filter by recipient agent")]
        agent: Option<String>,
        #[arg(long, help = "Filter by sender")]
        from_agent: Option<String>,
        #[arg(
            long,
            default_value_t = 10080,
            help = "Time window in minutes [max 10080 = 7 days]"
        )]
        since_minutes: u64,
        #[arg(long, default_value_t = 10000, help = "Max messages to export")]
        limit: usize,
    },

    /// List historical presence events from `PostgreSQL`.
    #[command(long_about = "Query PostgreSQL for historical presence events.\n\
        Unlike presence-list (which shows current TTL-based state from Redis),\n\
        this shows the full history of presence changes.")]
    PresenceHistory {
        #[arg(long, help = "Filter by agent ID")]
        agent: Option<String>,
        #[arg(long, default_value_t = 1440, help = "Time window in minutes")]
        since_minutes: u64,
        #[arg(long, default_value_t = 50, help = "Max records")]
        limit: usize,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Export bus messages to a local NDJSON journal file (per-repo archive).
    #[command(
        long_about = "Export messages matching a filter to a local NDJSON file.\n\n\
        Idempotent: only appends messages not already in the file.\n\
        Use --tag to filter by repo (e.g., 'repo:stm32-merge').\n\
        Use --from-agent to filter by sender (e.g., 'codex').\n\n\
        Journal files are one JSON message per line, suitable for grep/jq analysis."
    )]
    Journal {
        #[arg(long, help = "Filter by tag (e.g., 'repo:stm32-merge')")]
        tag: Option<String>,
        #[arg(long, help = "Filter by sender agent")]
        from_agent: Option<String>,
        #[arg(
            long,
            default_value_t = 10080,
            help = "Time window in minutes [default: 7 days]"
        )]
        since_minutes: u64,
        #[arg(long, default_value_t = 10000, help = "Max messages to export")]
        limit: usize,
        #[arg(long, help = "Output NDJSON file path")]
        output: String,
    },

    /// List messages waiting for acknowledgement.
    #[command(
        long_about = "Scan Redis for all messages sent with --request-ack that have not yet\n\
            been acknowledged.  Entries older than 60 seconds are flagged STALE.\n\
            Entries expire automatically after 300 seconds.\n\n\
            HTTP endpoint: GET /pending-acks (same data via HTTP server)"
    )]
    PendingAcks {
        #[arg(long, help = "Filter by recipient agent ID")]
        agent: Option<String>,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Sync findings between Claude and Codex sessions.
    #[command(
        long_about = "Discover Codex configuration from ~/.codex/config.toml, read recent\n\
            bus messages addressed to the Codex agent, and normalize any finding-type\n\
            messages.  Outputs a summary JSON with counts and per-finding details.\n\n\
            This command reads from the bus but does not post any messages.  Use\n\
            'agent-bus send' to route formatted findings to Codex after reviewing the\n\
            sync output."
    )]
    CodexSync {
        #[arg(long, default_value_t = 100, help = "Max messages to read per agent")]
        limit: usize,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Backfill Redis messages into `PostgreSQL` (one-time sync).
    #[command(
        long_about = "Read all messages from the Redis stream and insert any missing\n\
        ones into PostgreSQL. Safe to run multiple times (uses ON CONFLICT DO NOTHING).\n\
        Use after enabling PostgreSQL on an existing Redis deployment."
    )]
    Sync {
        #[arg(
            long,
            default_value_t = 100_000,
            help = "Max messages to read from Redis"
        )]
        limit: usize,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Real-time monitoring of active agent sessions.
    #[command(
        long_about = "Render a continuously-refreshing terminal dashboard showing per-agent\n\
            message counts, completion state, and aggregate finding severity counts\n\
            (CRITICAL/HIGH/MEDIUM/LOW).\n\n\
            Filter by session tag to focus on a single coordination session.\n\
            Runs until interrupted (Ctrl-C)."
    )]
    Monitor {
        #[arg(
            long,
            help = "Filter by session tag (e.g. 'session:framework-upgrade')"
        )]
        session: Option<String>,
        #[arg(
            long,
            default_value_t = 5,
            help = "Refresh interval in seconds [1-3600]"
        )]
        refresh: u64,
    },

    /// Run as an MCP server (stdio transport), HTTP REST server, or MCP Streamable HTTP server.
    #[command(
        long_about = "Start a Model Context Protocol (MCP) server on stdio, an HTTP REST server,\n\
        or an MCP Streamable HTTP server (spec 2025-06-18).\n\n\
        MCP stdio mode (default, --transport stdio):\n\
        Exposes 8 tools to LLM agents: bus_health, post_message, list_messages,\n\
        ack_message, set_presence, list_presence, list_presence_history, negotiate.\n\
        Register in mcp.json / config.toml / settings.json:\n\
        {\"command\": \"agent-bus\", \"args\": [\"serve\", \"--transport\", \"stdio\"]}\n\n\
        HTTP REST mode (--transport http, default port 8400):\n\
        Starts an axum HTTP server on localhost:PORT.\n\
        Routes: GET /health, POST /messages, GET /messages, POST /messages/:id/ack,\n\
        PUT /presence/:agent, GET /presence, GET /events (SSE)\n\n\
        MCP Streamable HTTP mode (--transport mcp-http, default port 8401):\n\
        Implements the MCP Streamable HTTP transport spec (2025-06-18).\n\
        Routes: POST /mcp (JSON-RPC tool dispatch), GET /mcp (capability discovery).\n\
        Supports SSE streaming when client sends Accept: text/event-stream.\n\
        Session continuity via Mcp-Session-Id response header.\n\n\
        On startup, announces presence and posts a startup message if\n\
        AGENT_BUS_STARTUP_ENABLED=true (default)."
    )]
    Serve {
        #[arg(
            long,
            default_value = "stdio",
            help = "Transport protocol: stdio|http|mcp-http"
        )]
        transport: String,
        #[arg(
            long,
            default_value_t = 8400,
            help = "Server port (http default: 8400; mcp-http should use 8401)"
        )]
        port: u16,
    },

    /// Send multiple messages from a NDJSON file (one JSON object per line).
    #[command(
        long_about = "Read a newline-delimited JSON file where each line is a message object\n\
        with fields: sender, recipient, topic, body (and optionally: tags, priority,\n\
        thread_id, request_ack, reply_to, metadata, schema).\n\n\
        Each message is validated and posted. Results (message IDs) are printed\n\
        to stdout one per line.\n\n\
        Example NDJSON line:\n\
        {\"sender\":\"euler\",\"recipient\":\"claude\",\"topic\":\"status\",\"body\":\"done\",\"tags\":[\"repo:agent-hub\"]}"
    )]
    BatchSend {
        #[arg(long, help = "Path to NDJSON file (use '-' for stdin)")]
        file: String,
        #[arg(long, default_value = "compact", help = "Output format for results")]
        encoding: Encoding,
    },

    // -----------------------------------------------------------------------
    // Channel commands
    // -----------------------------------------------------------------------
    /// Send a direct private message to another agent.
    #[command(
        long_about = "Post a message to the private direct channel between two agents.\n\n\
        Stored in Redis stream 'bus:direct:<a>:<b>' (agents sorted alphabetically).\n\
        Use for private coordination that should not appear in the shared bus stream."
    )]
    PostDirect {
        #[arg(long, help = "Sender agent ID")]
        from_agent: String,
        #[arg(long, help = "Recipient agent ID")]
        to_agent: String,
        #[arg(long, default_value = "direct", help = "Message topic")]
        topic: String,
        #[arg(long, help = "Message body")]
        body: String,
        #[arg(long, help = "Optional thread ID")]
        thread_id: Option<String>,
        #[arg(long, action = clap::ArgAction::Append, help = "Tag (repeatable)")]
        tag: Vec<String>,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Read direct messages between two agents.
    #[command(
        long_about = "Read messages from the private direct channel between two agents.\n\n\
        Returns messages in chronological order (oldest first), up to --limit."
    )]
    ReadDirect {
        #[arg(long, help = "First agent ID")]
        agent_a: String,
        #[arg(long, help = "Second agent ID")]
        agent_b: String,
        #[arg(long, default_value_t = 50, help = "Max messages to return [1-500]")]
        limit: usize,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Post a message to a named group channel.
    #[command(long_about = "Post a message to a named group discussion channel.\n\n\
        The group must already exist (create it first via the HTTP API or MCP tool).\n\
        Stored in Redis stream 'bus:group:<name>'.")]
    PostGroup {
        #[arg(long, help = "Group name (alphanumerics, hyphens, underscores)")]
        group: String,
        #[arg(long, help = "Sender agent ID")]
        from_agent: String,
        #[arg(long, default_value = "group", help = "Message topic")]
        topic: String,
        #[arg(long, help = "Message body")]
        body: String,
        #[arg(long, help = "Optional thread ID")]
        thread_id: Option<String>,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Read messages from a named group channel.
    #[command(
        long_about = "Read messages from a named group discussion channel.\n\n\
        Returns messages in chronological order (oldest first), up to --limit."
    )]
    ReadGroup {
        #[arg(long, help = "Group name")]
        group: String,
        #[arg(long, default_value_t = 50, help = "Max messages to return [1-500]")]
        limit: usize,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Claim ownership of a resource (file, directory, etc.).
    #[command(long_about = "Claim first-edit ownership of a resource.\n\n\
        First claim is auto-granted. Subsequent claims from other agents trigger\n\
        arbitration: both claimants are notified and the orchestrator receives an\n\
        escalation message to resolve the conflict.\n\n\
        Claims expire automatically after 1 hour.\n\n\
        Examples:\n  \
        agent-bus claim src/redis_bus.rs --agent claude --reason \"Adding compression\"\n  \
        agent-bus claim src/ --agent codex --reason \"Refactoring module layout\"")]
    Claim {
        #[arg(help = "Resource path to claim (e.g. src/redis_bus.rs)")]
        resource: String,
        #[arg(long, help = "Your agent ID")]
        agent: String,
        #[arg(
            long,
            default_value = "first-edit required",
            help = "Why you need first-edit"
        )]
        reason: String,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// List ownership claims, optionally filtered by resource or status.
    #[command(long_about = "List all outstanding ownership claims.\n\n\
        Filter by --resource to see claims for a specific file.\n\
        Filter by --status to see only contested claims:\n\
          status values: pending|granted|contested|review_assigned")]
    Claims {
        #[arg(long, help = "Filter by resource path")]
        resource: Option<String>,
        #[arg(
            long,
            help = "Filter by status: pending|granted|contested|review_assigned"
        )]
        status: Option<String>,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Resolve a contested ownership claim by naming a winner.
    #[command(long_about = "Resolve a contested ownership claim.\n\n\
        The winner receives 'granted' status and may proceed with first-edit.\n\
        Losing agents receive 'review_assigned' status and are notified via\n\
        direct message. A resolution reason is stored for audit.\n\n\
        Example:\n  \
        agent-bus resolve src/redis_bus.rs --winner claude \\\n  \
          --reason \"claude owns compression\" --resolved-by orchestrator")]
    Resolve {
        #[arg(help = "Resource path to resolve")]
        resource: String,
        #[arg(long, help = "Winning agent ID")]
        winner: String,
        #[arg(
            long,
            default_value = "resolved by orchestrator",
            help = "Resolution rationale"
        )]
        reason: String,
        #[arg(
            long,
            default_value = "orchestrator",
            help = "Who is making this decision"
        )]
        resolved_by: String,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    // -----------------------------------------------------------------------
    // Session management commands (Task 4.2 / 4.3)
    // -----------------------------------------------------------------------

    /// Summarize all messages in a session.
    #[command(
        long_about = "Read all messages tagged with 'session:<id>' and produce a JSON summary.\n\n\
            Reports: agents involved, topic distribution, severity counts, and time range.\n\
            Useful for post-session analysis and handoff documentation.\n\n\
            Example:\n  \
            agent-bus session-summary --session sprint-42 --encoding json"
    )]
    SessionSummary {
        #[arg(long, help = "Session ID to summarize (matches tag 'session:<id>')")]
        session: String,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Deduplicate findings across messages, grouped by file path.
    #[command(
        long_about = "Read recent messages and group findings by file path.\n\n\
            For each file path found in message bodies (any token containing\n\
            '/' or '\\' with a file extension), reports: finding count, max\n\
            severity (CRITICAL > HIGH > MEDIUM > LOW), and contributing agents.\n\n\
            Filter by --session to scope to a specific coordination session.\n\
            Filter by --agent to restrict to messages from one sender.\n\n\
            Example:\n  \
            agent-bus dedup --session sprint-42 --since-minutes 480"
    )]
    Dedup {
        #[arg(
            long,
            help = "Restrict to messages tagged with 'session:<id>'"
        )]
        session: Option<String>,
        #[arg(long, help = "Restrict to messages from this sender agent")]
        agent: Option<String>,
        #[arg(
            long,
            default_value_t = 1440,
            help = "Look back this many minutes [default: 1 day]"
        )]
        since_minutes: u64,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },
}
