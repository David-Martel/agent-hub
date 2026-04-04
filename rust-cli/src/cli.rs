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
        AGENT_BUS_DATABASE_URL       PostgreSQL connection URL [default: postgresql://postgres@localhost:5300/redis_backend]\n  \
        AGENT_BUS_STREAM_KEY         Redis Stream key [default: agent_bus:messages]\n  \
        AGENT_BUS_CHANNEL            Redis Pub/Sub channel [default: agent_bus:events]\n  \
        AGENT_BUS_PRESENCE_PREFIX    Redis key prefix for presence [default: agent_bus:presence:]\n  \
        AGENT_BUS_STREAM_MAXLEN      Max stream entries [default: 100000]\n  \
        AGENT_BUS_SERVER_HOST        HTTP bind host [default: localhost]\n  \
        AGENT_BUS_SERVICE_AGENT_ID   Agent ID for this service [default: agent-bus]\n  \
        AGENT_BUS_SERVICE_NAME       Windows service name for maintenance control [default: AgentHub]\n  \
        AGENT_BUS_STARTUP_ENABLED    Announce on MCP startup [default: true]\n  \
        AGENT_BUS_STARTUP_RECIPIENT  Startup message recipient [default: all]\n  \
        AGENT_BUS_STARTUP_TOPIC      Startup message topic [default: status]\n  \
        AGENT_BUS_STARTUP_BODY       Startup message body\n  \
        AGENT_BUS_SESSION_ID         Auto-tag all messages with session:<id>\n  \
        AGENT_BUS_SERVER_URL         Route CLI commands through this HTTP server (e.g. http://localhost:8400)\n  \
        AGENT_BUS_MACHINE_SAFE      Suppress non-fatal degraded fallback warnings in machine-readable output [default: false]\n  \
        AGENT_BUS_CONFIG             Config file path [default: ~/.config/agent-bus/config.json]\n  \
        RUST_LOG                     Log level filter [default: error]\n\n\
        ENCODING MODES:\n  \
        compact   Machine-optimized JSON, no whitespace (default for scripts/CI)\n  \
        json      Pretty-printed JSON with 2-space indent (debugging)\n  \
        minimal   Token-minimized: short field names, defaults stripped (~50% fewer tokens)\n  \
        toon      Token-Optimized Object Notation: @from→to #topic [tags] body (~70% fewer tokens)\n  \
        human     Table format for terminal reading (read/watch only)\n\n\
        DOCUMENTATION:\n  \
        Protocol spec:    AGENT_COMMUNICATIONS.md (deployed to repo roots via journal)\n  \
        Project docs:     C:\\codedev\\agent-bus\\AGENT_COMMUNICATIONS.md\n  \
        Rust dev guide:   ~/.agents/rust-development-guide.md\n  \
        Impl notes:       C:\\codedev\\agent-bus\\IMPLEMENTATION_NOTES.md\n\n\
        EXAMPLES:\n  \
        agent-bus health\n  \
        agent-bus send --from-agent claude --to-agent codex --topic status --body \"ready\"\n  \
        agent-bus read --agent claude --since-minutes 60 --encoding human\n  \
        agent-bus watch --agent claude --history 10 --encoding human\n  \
        agent-bus ack --agent claude --message-id <UUID>\n  \
        agent-bus presence --agent claude --capability mcp --capability rust\n  \
        agent-bus claim --agent claude --resource src/main.rs --reason \"refactoring\"\n  \
        agent-bus service --action status\n  \
        agent-bus service --action restart --reason \"deploy new binary\"\n  \
        agent-bus session-summary --session my-session-id\n  \
        agent-bus dedup --session my-session-id --encoding json\n  \
        agent-bus token-count --text \"estimate this text\"\n  \
        agent-bus compact-context --max-tokens 4000 --since-minutes 60\n  \
        agent-bus serve --transport stdio  # MCP server mode for mcp.json\n  \
        agent-bus serve --transport http --port 8400  # HTTP REST + SSE"
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
        Use --repo, --session, --tag, and --thread-id for message metadata filters.\n\
        Broadcast messages (to='all') are included by default.")]
    Read {
        #[arg(long, help = "Filter by recipient agent ID")]
        agent: Option<String>,
        #[arg(long, help = "Filter by sender agent ID")]
        from_agent: Option<String>,
        #[arg(long, help = "Filter by repository tag value (matches repo:<value>)")]
        repo: Option<String>,
        #[arg(long, help = "Filter by session tag value (matches session:<value>)")]
        session: Option<String>,
        #[arg(long, action = clap::ArgAction::Append, help = "Filter by tag value (repeatable)")]
        tag: Vec<String>,
        #[arg(long, help = "Filter by exact thread ID")]
        thread_id: Option<String>,
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
        #[arg(
            long,
            help = "Truncate message bodies to N chars and append a truncation marker"
        )]
        excerpt: Option<usize>,
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
        Useful for backup, recovery, and analysis workflows.\n\
        Supports the same repo/session/tag/thread-id filters as `read`.")]
    Export {
        #[arg(long, help = "Filter by recipient agent")]
        agent: Option<String>,
        #[arg(long, help = "Filter by sender")]
        from_agent: Option<String>,
        #[arg(long, help = "Filter by repository tag value (matches repo:<value>)")]
        repo: Option<String>,
        #[arg(long, help = "Filter by session tag value (matches session:<value>)")]
        session: Option<String>,
        #[arg(long, action = clap::ArgAction::Append, help = "Filter by tag value (repeatable)")]
        tag: Vec<String>,
        #[arg(long, help = "Filter by exact thread ID")]
        thread_id: Option<String>,
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
        Use --repo to filter by repository name and --session to filter by session.\n\
        Use --tag for additional tags (repeatable).\n\
        Use --from-agent to filter by sender (e.g., 'codex').\n\n\
        Journal files are one JSON message per line, suitable for grep/jq analysis."
    )]
    Journal {
        #[arg(long, help = "Filter by repository tag value (matches repo:<value>)")]
        repo: Option<String>,
        #[arg(long, help = "Filter by session tag value (matches session:<value>)")]
        session: Option<String>,
        #[arg(long, action = clap::ArgAction::Append, help = "Filter by tag value (repeatable)")]
        tag: Vec<String>,
        #[arg(long, help = "Filter by exact thread ID")]
        thread_id: Option<String>,
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
        Exposes 13 tools to LLM agents: bus_health, post_message, list_messages,\n\
        ack_message, set_presence, list_presence, list_presence_history, negotiate,\n\
        create_channel, post_to_channel, read_channel, claim_resource, resolve_claim.\n\
        Register in mcp.json / config.toml / settings.json:\n\
        {\"command\": \"agent-bus\", \"args\": [\"serve\", \"--transport\", \"stdio\"]}\n\n\
        HTTP REST mode (--transport http, default port 8400):\n\
        Starts an axum HTTP server on localhost:PORT.\n\
        Routes: GET /health, POST /messages, GET /messages, POST /messages/:id/ack,\n\
        PUT /presence/:agent, GET /presence, GET /events (SSE),\n\
        POST /token-count, POST /compact-context, /channels/*, /pending-acks\n\n\
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

    /// Control maintenance state and Windows service lifecycle.
    #[command(
        long_about = "Control the running HTTP service for maintenance workflows.\n\n\
        Actions:\n\
        - status: read service/maintenance state from the HTTP admin endpoint.\n\
        - pause: reject mutating HTTP requests while allowing reads/SSE/health.\n\
        - resume: leave maintenance mode.\n\
        - flush: request an immediate PostgreSQL writer flush.\n\
        - stop: gracefully stop the running HTTP server.\n\
        - start: start the configured Windows service.\n\
        - restart: pause + flush + restart the configured Windows service.\n\n\
        The command uses AGENT_BUS_SERVER_URL when set; otherwise it defaults to\n\
        http://localhost:8400 for admin requests. Windows service actions use\n\
        AGENT_BUS_SERVICE_NAME (default: AgentHub)."
    )]
    Service {
        #[arg(
            long,
            default_value = "status",
            help = "Action: status|pause|resume|flush|stop|start|restart"
        )]
        action: String,
        #[arg(long, help = "Maintenance reason or operator note")]
        reason: Option<String>,
        #[arg(
            long,
            help = "Override HTTP base URL (default: AGENT_BUS_SERVER_URL or http://localhost:8400)"
        )]
        base_url: Option<String>,
        #[arg(long, help = "Override Windows service name")]
        service_name: Option<String>,
        #[arg(
            long,
            default_value_t = 20,
            help = "Timeout for service state transitions / health wait"
        )]
        timeout_seconds: u64,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
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
        #[arg(
            long,
            default_value = "exclusive",
            help = "Lease mode: shared|shared_namespaced|exclusive"
        )]
        mode: String,
        #[arg(
            long,
            help = "Namespace for shared_namespaced claims (e.g. cargo target dir)"
        )]
        namespace: Option<String>,
        #[arg(
            long,
            help = "Scope kind: repo_path|cross_repo_path|service|install_target|artifact_root|user_config|external"
        )]
        scope_kind: Option<String>,
        #[arg(long, help = "Canonical path or backing path for the resource")]
        scope_path: Option<String>,
        #[arg(long, action = clap::ArgAction::Append, help = "Affected repo scope (repeatable)")]
        repo_scope: Vec<String>,
        #[arg(long, help = "Coordination thread ID")]
        thread_id: Option<String>,
        #[arg(long, default_value_t = 3600, help = "Lease TTL in seconds [1-86400]")]
        lease_ttl_seconds: u64,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Renew an existing resource claim/lease before it expires.
    #[command(
        long_about = "Extend the TTL of an existing claim/lease held by an agent.\n\n\
        Useful for long-running work so stale claims expire automatically when an agent disappears."
    )]
    RenewClaim {
        #[arg(help = "Resource path to renew")]
        resource: String,
        #[arg(long, help = "Agent holding the claim")]
        agent: String,
        #[arg(
            long,
            default_value_t = 3600,
            help = "New lease TTL in seconds [1-86400]"
        )]
        lease_ttl_seconds: u64,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Release an existing resource claim/lease.
    #[command(long_about = "Release a claim/lease held by an agent.\n\n\
        Remaining compatible claimants are automatically downgraded/upgraded based on the lease mode rules.")]
    ReleaseClaim {
        #[arg(help = "Resource path to release")]
        resource: String,
        #[arg(long, help = "Agent releasing the claim")]
        agent: String,
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

    /// Send a high-priority attention signal to another agent.
    #[command(long_about = "Post a targeted high-priority knock message.\n\n\
        Knocks are durable direct notifications intended to make an active agent check its inbox or SSE stream immediately.")]
    Knock {
        #[arg(long, help = "Sender agent ID")]
        from_agent: String,
        #[arg(long, help = "Recipient agent ID")]
        to_agent: String,
        #[arg(long, default_value = "check the bus", help = "Knock body text")]
        body: String,
        #[arg(long, help = "Optional coordination thread ID")]
        thread_id: Option<String>,
        #[arg(long, action = clap::ArgAction::Append, help = "Tag (repeatable)")]
        tag: Vec<String>,
        #[arg(
            long,
            default_value_t = true,
            help = "Request acknowledgement from recipient"
        )]
        request_ack: bool,
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
        #[arg(long, help = "Restrict to messages tagged with 'session:<id>'")]
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

    /// Estimate the token count for a text string or stdin input.
    TokenCount {
        #[arg(long, help = "Text to estimate (reads from stdin when omitted)")]
        text: Option<String>,
        #[arg(long, default_value = "compact", value_enum)]
        encoding: Encoding,
    },

    /// Compact recent bus messages to fit within a token budget.
    CompactContext {
        #[arg(long, help = "Agent ID to read messages for")]
        agent: Option<String>,
        #[arg(long, help = "Filter by repository tag value (matches repo:<value>)")]
        repo: Option<String>,
        #[arg(long, help = "Filter by session tag value (matches session:<value>)")]
        session: Option<String>,
        #[arg(long, action = clap::ArgAction::Append, help = "Filter by tag value (repeatable)")]
        tag: Vec<String>,
        #[arg(long, help = "Filter by exact thread ID")]
        thread_id: Option<String>,
        #[arg(long, default_value_t = 60)]
        since_minutes: u64,
        #[arg(long, default_value_t = 4000)]
        max_tokens: usize,
        #[arg(long, default_value = "compact", value_enum)]
        encoding: Encoding,
    },

    /// Summarize messages within a single thread.
    ///
    /// Thread-scoped analogue of session-summary: retrieves messages with
    /// the given `thread_id` and produces an aggregated summary (agents,
    /// topics, severity, time range).
    #[command(
        long_about = "Fetch messages tagged with a specific thread_id and produce a\n\
            compact aggregated summary.\n\n\
            Reports: agents involved, topic distribution, severity counts, and\n\
            time range — scoped to the single thread.\n\n\
            Example:\n  \
            agent-bus summarize-thread --thread-id review-42 --encoding json"
    )]
    SummarizeThread {
        #[arg(long, help = "Thread ID to summarize (required)")]
        thread_id: String,
        #[arg(
            long,
            default_value_t = 10_080,
            help = "Look back this many minutes [default: 7 days]"
        )]
        since_minutes: u64,
        #[arg(
            long,
            default_value_t = 10_000,
            help = "Max messages to consider [default: 10000]"
        )]
        limit: usize,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },

    /// Compact messages within a single thread to fit a token budget.
    ///
    /// Thread-scoped analogue of compact-context: fetches messages from the
    /// live Redis stream filtered to the given `thread_id`, then trims to
    /// the token budget (newest messages preferred).
    #[command(
        long_about = "Fetch recent messages for a specific thread_id from the live\n\
            Redis stream and compact them to fit within a token budget.\n\n\
            Useful for injecting thread-scoped context into an LLM prompt\n\
            without exceeding the token window.\n\n\
            Example:\n  \
            agent-bus compact-thread --thread-id review-42 --token-budget 2000"
    )]
    CompactThread {
        #[arg(long, help = "Thread ID to compact (required)")]
        thread_id: String,
        #[arg(long, default_value_t = 4000, help = "Token budget [default: 4000]")]
        token_budget: usize,
        #[arg(
            long,
            default_value_t = 60,
            help = "Look back this many minutes [default: 60]"
        )]
        since_minutes: u64,
        #[arg(
            long,
            default_value_t = 500,
            help = "Max messages to fetch before compaction [default: 500]"
        )]
        limit: usize,
        #[arg(long, default_value = "compact", value_enum)]
        encoding: Encoding,
    },

    // -----------------------------------------------------------------------
    // Task queue commands
    // -----------------------------------------------------------------------
    /// Push a task to an agent's task queue.
    ///
    /// Orchestrators use this to dispatch work items to idle agents.  The
    /// queue is a Redis LIST (RPUSH); agents consume with `pull-task`.
    #[command(
        long_about = "Push a task JSON string (or plain text) to the tail of the target\n\
            agent's task queue. Tasks are stored as a Redis LIST under the key\n\
            'bus:tasks:<agent>'. Returns the new queue length.\n\n\
            Use 'pull-task' to consume the next task and 'peek-tasks' to\n\
            inspect pending work without consuming it.\n\n\
            Example:\n  \
            agent-bus push-task --agent codex --task '{\"type\":\"review\",\"file\":\"src/main.rs\"}'"
    )]
    PushTask {
        #[arg(long, help = "Target agent ID (e.g. codex, euler)")]
        agent: String,
        #[arg(long, help = "Task payload (JSON object or plain text)")]
        task: String,
        #[arg(long, default_value = "compact", value_enum, help = "Output format")]
        encoding: Encoding,
        #[arg(long, help = "Repository this task relates to")]
        repo: Option<String>,
        #[arg(
            long,
            default_value = "normal",
            help = "Priority: normal, high, or critical"
        )]
        priority: String,
        #[arg(long, value_delimiter = ',', help = "Comma-separated tags")]
        tags: Vec<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Comma-separated task IDs this depends on"
        )]
        depends_on: Vec<String>,
        #[arg(long, help = "Message ID this task is a reply to")]
        reply_to: Option<String>,
        #[arg(long, default_value = "cli", help = "Agent or user creating this task")]
        created_by: String,
    },

    /// Pull the next task from an agent's queue (destructive).
    ///
    /// Consumes and returns the oldest pending task.  Returns `null` when
    /// the queue is empty.
    #[command(
        long_about = "Pop and return the next task from the head of an agent's task\n\
            queue (LPOP).  The task is permanently removed from the queue.\n\n\
            Returns JSON: {\"agent\": \"<id>\", \"task\": \"<payload>\" | null}\n\n\
            Example:\n  \
            agent-bus pull-task --agent codex"
    )]
    PullTask {
        #[arg(long, help = "Agent ID to pull tasks for")]
        agent: String,
        #[arg(long, default_value = "compact", value_enum, help = "Output format")]
        encoding: Encoding,
    },

    /// Peek at pending tasks without consuming them.
    ///
    /// Returns the first `--limit` tasks from the queue head without removing
    /// them.  Use `pull-task` to actually consume work.
    #[command(
        long_about = "Read up to --limit tasks from the head of an agent's queue\n\
            without consuming them (LRANGE).  Use --limit 0 to return all entries.\n\n\
            Returns JSON: {\"agent\": \"<id>\", \"tasks\": [...], \"count\": N}\n\n\
            Example:\n  \
            agent-bus peek-tasks --agent codex --limit 5 --encoding json"
    )]
    PeekTasks {
        #[arg(long, help = "Agent ID to inspect")]
        agent: String,
        #[arg(long, default_value_t = 10, help = "Max tasks to return (0 = all)")]
        limit: usize,
        #[arg(long, default_value = "compact", value_enum, help = "Output format")]
        encoding: Encoding,
    },

    // -----------------------------------------------------------------------
    // Subscription commands
    // -----------------------------------------------------------------------
    /// Register interest in specific message scopes.
    ///
    /// Creates a subscription record so the bus knows what this agent cares
    /// about.  Scope filters are combined with AND across categories and OR
    /// within each category's list.
    #[command(
        long_about = "Create a subscription that declares which messages an agent is\n\
            interested in. Scope filters (repo, session, thread, tag, topic,\n\
            priority, resource) are combined with AND across categories and\n\
            OR within each category's list.\n\n\
            Subscriptions are metadata records stored in Redis.  When a TTL is\n\
            specified, the subscription auto-expires after that many seconds.\n\n\
            Examples:\n  \
            agent-bus subscribe --agent claude --repo agent-bus --topic status\n  \
            agent-bus subscribe --agent codex --tag urgent --priority-min high --ttl 3600"
    )]
    Subscribe {
        #[arg(long, help = "Subscribing agent ID")]
        agent: String,
        #[arg(long, value_delimiter = ',', help = "Repos to match (comma-separated)")]
        repo: Vec<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Sessions to match (comma-separated)"
        )]
        session: Vec<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Thread IDs to match (comma-separated)"
        )]
        thread: Vec<String>,
        #[arg(long, value_delimiter = ',', help = "Tags to match (comma-separated)")]
        tag: Vec<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Topics to match (comma-separated)"
        )]
        topic: Vec<String>,
        #[arg(long, help = "Minimum priority threshold: low, normal, high, urgent")]
        priority_min: Option<String>,
        #[arg(
            long,
            value_delimiter = ',',
            help = "Resources to match (comma-separated)"
        )]
        resource: Vec<String>,
        #[arg(long, help = "Auto-expire after this many seconds")]
        ttl: Option<u64>,
        #[arg(long, default_value = "compact", value_enum, help = "Output format")]
        encoding: Encoding,
    },

    /// Remove a subscription by ID.
    #[command(
        long_about = "Delete a subscription record from Redis.  The subscription ID is\n\
            the UUID returned by the `subscribe` command.\n\n\
            Returns {\"deleted\": true} if the subscription existed and was removed,\n\
            or {\"deleted\": false} if it was already expired or never existed.\n\n\
            Example:\n  \
            agent-bus unsubscribe --agent claude --id <subscription-uuid>"
    )]
    Unsubscribe {
        #[arg(long, help = "Agent that owns the subscription")]
        agent: String,
        #[arg(long, help = "Subscription UUID to remove")]
        id: String,
        #[arg(long, default_value = "compact", value_enum, help = "Output format")]
        encoding: Encoding,
    },

    /// List an agent's active subscriptions.
    #[command(
        long_about = "List all active subscriptions for a given agent.  Expired\n\
            subscriptions (past their TTL) are automatically excluded.\n\n\
            Returns JSON: {\"agent\": \"<id>\", \"subscriptions\": [...], \"count\": N}\n\n\
            Example:\n  \
            agent-bus subscriptions --agent claude --encoding json"
    )]
    Subscriptions {
        #[arg(long, help = "Agent ID to list subscriptions for")]
        agent: String,
        #[arg(long, default_value = "compact", value_enum, help = "Output format")]
        encoding: Encoding,
    },

    // -----------------------------------------------------------------------
    // Inventory commands
    // -----------------------------------------------------------------------
    /// Show active repos, sessions, and agents on the bus.
    #[command(
        long_about = "Scan presence records and recent messages for unique repo:* and session:*\n\
            tags. Returns a sorted inventory of active repositories and sessions.\n\n\
            Use --repo <name> to drill into a specific repository: shows agents that\n\
            have presence or recent messages tagged with that repo, plus any open\n\
            ownership claims whose resource matches the repo name.\n\n\
            Examples:\n  \
            agent-bus inventory                        # list all repos + sessions\n  \
            agent-bus inventory --repo agent-bus       # agents + claims for a repo\n  \
            agent-bus inventory --encoding json        # pretty-printed JSON output"
    )]
    Inventory {
        #[arg(long, help = "Drill into a specific repository name")]
        repo: Option<String>,
        #[arg(long, default_value = "compact", help = "Output format")]
        encoding: Encoding,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    // ------------------------------------------------------------------
    // health
    // ------------------------------------------------------------------

    #[test]
    fn parse_health_minimal() {
        let cli = parse(&["agent-bus", "health"]).expect("health must parse");
        assert!(matches!(cli.command, Cmd::Health { .. }));
    }

    #[test]
    fn parse_health_explicit_encoding() {
        let cli = parse(&["agent-bus", "health", "--encoding", "json"])
            .expect("health --encoding json must parse");
        assert!(matches!(cli.command, Cmd::Health { .. }));
    }

    // ------------------------------------------------------------------
    // send
    // ------------------------------------------------------------------

    #[test]
    fn parse_send_required_args() {
        let cli = parse(&[
            "agent-bus",
            "send",
            "--from-agent",
            "claude",
            "--to-agent",
            "codex",
            "--topic",
            "status",
            "--body",
            "hello",
        ])
        .expect("send with required args must parse");

        if let Cmd::Send {
            from_agent,
            to_agent,
            topic,
            body,
            ..
        } = cli.command
        {
            assert_eq!(from_agent, "claude");
            assert_eq!(to_agent, "codex");
            assert_eq!(topic, "status");
            assert_eq!(body, "hello");
        } else {
            panic!("expected Cmd::Send");
        }
    }

    #[test]
    fn parse_send_optional_args() {
        let cli = parse(&[
            "agent-bus",
            "send",
            "--from-agent",
            "euler",
            "--to-agent",
            "all",
            "--topic",
            "review-findings",
            "--body",
            "FINDING: unused import SEVERITY: LOW",
            "--priority",
            "high",
            "--tag",
            "repo:agent-bus",
            "--tag",
            "session:s1",
            "--request-ack",
            "--thread-id",
            "thread-99",
            "--schema",
            "finding",
        ])
        .expect("send with optional args must parse");

        if let Cmd::Send {
            from_agent,
            to_agent,
            priority,
            tag,
            request_ack,
            thread_id,
            schema,
            ..
        } = cli.command
        {
            assert_eq!(from_agent, "euler");
            assert_eq!(to_agent, "all");
            assert_eq!(priority, "high");
            assert_eq!(tag, vec!["repo:agent-bus", "session:s1"]);
            assert!(request_ack);
            assert_eq!(thread_id, Some("thread-99".to_owned()));
            assert_eq!(schema, Some("finding".to_owned()));
        } else {
            panic!("expected Cmd::Send");
        }
    }

    #[test]
    fn parse_send_priority_string_is_accepted_by_parser() {
        // Validation of the priority value is done at runtime, not by clap.
        // A nonsense priority string must still parse successfully.
        let result = parse(&[
            "agent-bus",
            "send",
            "--from-agent",
            "a",
            "--to-agent",
            "b",
            "--topic",
            "t",
            "--body",
            "b",
            "--priority",
            "turbo-supreme",
        ]);
        assert!(
            result.is_ok(),
            "clap must not reject unknown priority strings"
        );
    }

    #[test]
    fn parse_send_missing_required_arg_fails() {
        // Missing --body must cause a parse error.
        let result = parse(&[
            "agent-bus",
            "send",
            "--from-agent",
            "a",
            "--to-agent",
            "b",
            "--topic",
            "t",
        ]);
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // read
    // ------------------------------------------------------------------

    #[test]
    fn parse_read_with_agent_and_since_minutes() {
        let cli = parse(&[
            "agent-bus",
            "read",
            "--agent",
            "claude",
            "--since-minutes",
            "60",
        ])
        .expect("read must parse");

        if let Cmd::Read {
            agent,
            since_minutes,
            ..
        } = cli.command
        {
            assert_eq!(agent, Some("claude".to_owned()));
            assert_eq!(since_minutes, 60);
        } else {
            panic!("expected Cmd::Read");
        }
    }

    #[test]
    fn parse_read_defaults() {
        let cli = parse(&["agent-bus", "read"]).expect("read with no args must parse");
        if let Cmd::Read {
            agent,
            since_minutes,
            limit,
            exclude_broadcast,
            repo,
            session,
            tag,
            thread_id,
            ..
        } = cli.command
        {
            assert!(agent.is_none());
            assert_eq!(since_minutes, 1440);
            assert_eq!(limit, 50);
            assert!(!exclude_broadcast);
            assert!(repo.is_none());
            assert!(session.is_none());
            assert!(tag.is_empty());
            assert!(thread_id.is_none());
        } else {
            panic!("expected Cmd::Read");
        }
    }

    #[test]
    fn parse_read_message_filters() {
        let cli = parse(&[
            "agent-bus",
            "read",
            "--repo",
            "agent-bus",
            "--session",
            "sprint-42",
            "--tag",
            "priority:high",
            "--tag",
            "kind:finding",
            "--thread-id",
            "thread-123",
        ])
        .expect("read with filters must parse");

        if let Cmd::Read {
            repo,
            session,
            tag,
            thread_id,
            ..
        } = cli.command
        {
            assert_eq!(repo, Some("agent-bus".to_owned()));
            assert_eq!(session, Some("sprint-42".to_owned()));
            assert_eq!(
                tag,
                vec!["priority:high".to_owned(), "kind:finding".to_owned()]
            );
            assert_eq!(thread_id, Some("thread-123".to_owned()));
        } else {
            panic!("expected Cmd::Read");
        }
    }

    #[test]
    fn parse_export_message_filters() {
        let cli = parse(&[
            "agent-bus",
            "export",
            "--repo",
            "agent-bus",
            "--session",
            "sprint-42",
            "--tag",
            "kind:finding",
            "--thread-id",
            "thread-123",
        ])
        .expect("export with filters must parse");

        if let Cmd::Export {
            repo,
            session,
            tag,
            thread_id,
            ..
        } = cli.command
        {
            assert_eq!(repo, Some("agent-bus".to_owned()));
            assert_eq!(session, Some("sprint-42".to_owned()));
            assert_eq!(tag, vec!["kind:finding".to_owned()]);
            assert_eq!(thread_id, Some("thread-123".to_owned()));
        } else {
            panic!("expected Cmd::Export");
        }
    }

    #[test]
    fn parse_journal_message_filters() {
        let cli = parse(&[
            "agent-bus",
            "journal",
            "--repo",
            "agent-bus",
            "--session",
            "sprint-42",
            "--tag",
            "kind:finding",
            "--tag",
            "priority:high",
            "--thread-id",
            "thread-123",
            "--output",
            "journal.ndjson",
        ])
        .expect("journal with filters must parse");

        if let Cmd::Journal {
            repo,
            session,
            tag,
            thread_id,
            output,
            ..
        } = cli.command
        {
            assert_eq!(repo, Some("agent-bus".to_owned()));
            assert_eq!(session, Some("sprint-42".to_owned()));
            assert_eq!(
                tag,
                vec!["kind:finding".to_owned(), "priority:high".to_owned()]
            );
            assert_eq!(thread_id, Some("thread-123".to_owned()));
            assert_eq!(output, "journal.ndjson");
        } else {
            panic!("expected Cmd::Journal");
        }
    }

    // ------------------------------------------------------------------
    // serve
    // ------------------------------------------------------------------

    #[test]
    fn parse_serve_stdio_default() {
        let cli =
            parse(&["agent-bus", "serve", "--transport", "stdio"]).expect("serve stdio must parse");
        if let Cmd::Serve { transport, port } = cli.command {
            assert_eq!(transport, "stdio");
            assert_eq!(port, 8400); // default port
        } else {
            panic!("expected Cmd::Serve");
        }
    }

    #[test]
    fn parse_serve_http_custom_port() {
        let cli = parse(&[
            "agent-bus",
            "serve",
            "--transport",
            "http",
            "--port",
            "9000",
        ])
        .expect("serve http --port 9000 must parse");
        if let Cmd::Serve { transport, port } = cli.command {
            assert_eq!(transport, "http");
            assert_eq!(port, 9000);
        } else {
            panic!("expected Cmd::Serve");
        }
    }

    #[test]
    fn parse_serve_mcp_http_transport() {
        let cli = parse(&[
            "agent-bus",
            "serve",
            "--transport",
            "mcp-http",
            "--port",
            "8401",
        ])
        .expect("serve mcp-http must parse");
        if let Cmd::Serve { transport, .. } = cli.command {
            assert_eq!(transport, "mcp-http");
        } else {
            panic!("expected Cmd::Serve");
        }
    }

    #[test]
    fn parse_serve_default_no_args() {
        // --transport has a default value of "stdio"
        let cli = parse(&["agent-bus", "serve"]).expect("serve with no args must parse");
        assert!(matches!(cli.command, Cmd::Serve { .. }));
    }

    #[test]
    fn parse_service_pause_action() {
        let cli = parse(&[
            "agent-bus",
            "service",
            "--action",
            "pause",
            "--reason",
            "deploy maintenance",
            "--base-url",
            "http://localhost:8400",
            "--service-name",
            "AgentHub",
            "--timeout-seconds",
            "45",
        ])
        .expect("service pause must parse");

        if let Cmd::Service {
            action,
            reason,
            base_url,
            service_name,
            timeout_seconds,
            ..
        } = cli.command
        {
            assert_eq!(action, "pause");
            assert_eq!(reason, Some("deploy maintenance".to_owned()));
            assert_eq!(base_url, Some("http://localhost:8400".to_owned()));
            assert_eq!(service_name, Some("AgentHub".to_owned()));
            assert_eq!(timeout_seconds, 45);
        } else {
            panic!("expected Cmd::Service");
        }
    }

    #[test]
    fn parse_service_restart_defaults() {
        let cli = parse(&["agent-bus", "service", "--action", "restart"])
            .expect("service restart must parse");

        if let Cmd::Service {
            action,
            reason,
            base_url,
            service_name,
            timeout_seconds,
            ..
        } = cli.command
        {
            assert_eq!(action, "restart");
            assert!(reason.is_none());
            assert!(base_url.is_none());
            assert!(service_name.is_none());
            assert_eq!(timeout_seconds, 20);
        } else {
            panic!("expected Cmd::Service");
        }
    }

    // ------------------------------------------------------------------
    // claim (positional resource arg)
    // ------------------------------------------------------------------

    #[test]
    fn parse_claim_positional_resource() {
        let cli = parse(&[
            "agent-bus",
            "claim",
            "src/redis_bus.rs",
            "--agent",
            "claude",
        ])
        .expect("claim with positional resource must parse");

        if let Cmd::Claim {
            resource,
            agent,
            reason,
            mode,
            namespace,
            lease_ttl_seconds,
            ..
        } = cli.command
        {
            assert_eq!(resource, "src/redis_bus.rs");
            assert_eq!(agent, "claude");
            assert_eq!(reason, "first-edit required"); // default
            assert_eq!(mode, "exclusive");
            assert!(namespace.is_none());
            assert_eq!(lease_ttl_seconds, 3600);
        } else {
            panic!("expected Cmd::Claim");
        }
    }

    #[test]
    fn parse_claim_with_custom_reason() {
        let cli = parse(&[
            "agent-bus",
            "claim",
            "src/",
            "--agent",
            "codex",
            "--reason",
            "full module refactor",
            "--mode",
            "shared_namespaced",
            "--namespace",
            "codex-target",
            "--repo-scope",
            "wezterm",
            "--thread-id",
            "wezterm-thread-1",
            "--lease-ttl-seconds",
            "900",
        ])
        .expect("claim with reason must parse");

        if let Cmd::Claim {
            resource,
            agent,
            reason,
            mode,
            namespace,
            repo_scope,
            thread_id,
            lease_ttl_seconds,
            ..
        } = cli.command
        {
            assert_eq!(resource, "src/");
            assert_eq!(agent, "codex");
            assert_eq!(reason, "full module refactor");
            assert_eq!(mode, "shared_namespaced");
            assert_eq!(namespace, Some("codex-target".to_owned()));
            assert_eq!(repo_scope, vec!["wezterm".to_owned()]);
            assert_eq!(thread_id, Some("wezterm-thread-1".to_owned()));
            assert_eq!(lease_ttl_seconds, 900);
        } else {
            panic!("expected Cmd::Claim");
        }
    }

    #[test]
    fn parse_claim_missing_agent_fails() {
        // --agent is required (no default)
        let result = parse(&["agent-bus", "claim", "src/main.rs"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_renew_claim() {
        let cli = parse(&[
            "agent-bus",
            "renew-claim",
            "src/main.rs",
            "--agent",
            "claude",
            "--lease-ttl-seconds",
            "600",
        ])
        .expect("renew-claim must parse");

        if let Cmd::RenewClaim {
            resource,
            agent,
            lease_ttl_seconds,
            ..
        } = cli.command
        {
            assert_eq!(resource, "src/main.rs");
            assert_eq!(agent, "claude");
            assert_eq!(lease_ttl_seconds, 600);
        } else {
            panic!("expected Cmd::RenewClaim");
        }
    }

    #[test]
    fn parse_release_claim() {
        let cli = parse(&[
            "agent-bus",
            "release-claim",
            "src/main.rs",
            "--agent",
            "claude",
        ])
        .expect("release-claim must parse");

        if let Cmd::ReleaseClaim {
            resource, agent, ..
        } = cli.command
        {
            assert_eq!(resource, "src/main.rs");
            assert_eq!(agent, "claude");
        } else {
            panic!("expected Cmd::ReleaseClaim");
        }
    }

    // ------------------------------------------------------------------
    // session-summary
    // ------------------------------------------------------------------

    #[test]
    fn parse_session_summary() {
        let cli = parse(&["agent-bus", "session-summary", "--session", "test-123"])
            .expect("session-summary must parse");

        if let Cmd::SessionSummary { session, .. } = cli.command {
            assert_eq!(session, "test-123");
        } else {
            panic!("expected Cmd::SessionSummary");
        }
    }

    #[test]
    fn parse_session_summary_missing_session_fails() {
        let result = parse(&["agent-bus", "session-summary"]);
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // token-count
    // ------------------------------------------------------------------

    #[test]
    fn parse_token_count_with_text() {
        let cli = parse(&["agent-bus", "token-count", "--text", "estimate this"])
            .expect("token-count --text must parse");

        if let Cmd::TokenCount { text, .. } = cli.command {
            assert_eq!(text, Some("estimate this".to_owned()));
        } else {
            panic!("expected Cmd::TokenCount");
        }
    }

    #[test]
    fn parse_token_count_without_text() {
        // text is optional — reads stdin when omitted
        let cli =
            parse(&["agent-bus", "token-count"]).expect("token-count with no args must parse");
        if let Cmd::TokenCount { text, .. } = cli.command {
            assert!(text.is_none());
        } else {
            panic!("expected Cmd::TokenCount");
        }
    }

    // ------------------------------------------------------------------
    // compact-context
    // ------------------------------------------------------------------

    #[test]
    fn parse_compact_context_all_options() {
        let cli = parse(&[
            "agent-bus",
            "compact-context",
            "--agent",
            "claude",
            "--repo",
            "wezterm",
            "--session",
            "wave-2",
            "--tag",
            "planning",
            "--thread-id",
            "wezterm-joint-plan-20260324",
            "--since-minutes",
            "120",
            "--max-tokens",
            "8000",
            "--encoding",
            "json",
        ])
        .expect("compact-context with all options must parse");

        if let Cmd::CompactContext {
            agent,
            repo,
            session,
            tag,
            thread_id,
            since_minutes,
            max_tokens,
            ..
        } = cli.command
        {
            assert_eq!(agent, Some("claude".to_owned()));
            assert_eq!(repo, Some("wezterm".to_owned()));
            assert_eq!(session, Some("wave-2".to_owned()));
            assert_eq!(tag, vec!["planning".to_owned()]);
            assert_eq!(thread_id, Some("wezterm-joint-plan-20260324".to_owned()));
            assert_eq!(since_minutes, 120);
            assert_eq!(max_tokens, 8000);
        } else {
            panic!("expected Cmd::CompactContext");
        }
    }

    #[test]
    fn parse_compact_context_defaults() {
        let cli = parse(&["agent-bus", "compact-context"])
            .expect("compact-context with no args must parse");
        if let Cmd::CompactContext {
            agent,
            repo,
            session,
            tag,
            thread_id,
            since_minutes,
            max_tokens,
            ..
        } = cli.command
        {
            assert!(agent.is_none());
            assert!(repo.is_none());
            assert!(session.is_none());
            assert!(tag.is_empty());
            assert!(thread_id.is_none());
            assert_eq!(since_minutes, 60);
            assert_eq!(max_tokens, 4000);
        } else {
            panic!("expected Cmd::CompactContext");
        }
    }

    // ------------------------------------------------------------------
    // summarize-thread
    // ------------------------------------------------------------------

    #[test]
    fn parse_summarize_thread() {
        let cli = parse(&["agent-bus", "summarize-thread", "--thread-id", "review-42"])
            .expect("summarize-thread must parse");

        if let Cmd::SummarizeThread {
            thread_id,
            since_minutes,
            limit,
            ..
        } = cli.command
        {
            assert_eq!(thread_id, "review-42");
            assert_eq!(since_minutes, 10_080);
            assert_eq!(limit, 10_000);
        } else {
            panic!("expected Cmd::SummarizeThread");
        }
    }

    #[test]
    fn parse_summarize_thread_missing_thread_id_fails() {
        let result = parse(&["agent-bus", "summarize-thread"]);
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // compact-thread
    // ------------------------------------------------------------------

    #[test]
    fn parse_compact_thread() {
        let cli = parse(&[
            "agent-bus",
            "compact-thread",
            "--thread-id",
            "review-42",
            "--token-budget",
            "2000",
        ])
        .expect("compact-thread must parse");

        if let Cmd::CompactThread {
            thread_id,
            token_budget,
            since_minutes,
            limit,
            ..
        } = cli.command
        {
            assert_eq!(thread_id, "review-42");
            assert_eq!(token_budget, 2000);
            assert_eq!(since_minutes, 60);
            assert_eq!(limit, 500);
        } else {
            panic!("expected Cmd::CompactThread");
        }
    }

    #[test]
    fn parse_compact_thread_defaults() {
        let cli = parse(&["agent-bus", "compact-thread", "--thread-id", "t-1"])
            .expect("compact-thread with defaults must parse");

        if let Cmd::CompactThread {
            token_budget,
            since_minutes,
            limit,
            ..
        } = cli.command
        {
            assert_eq!(token_budget, 4000);
            assert_eq!(since_minutes, 60);
            assert_eq!(limit, 500);
        } else {
            panic!("expected Cmd::CompactThread");
        }
    }

    #[test]
    fn parse_compact_thread_missing_thread_id_fails() {
        let result = parse(&["agent-bus", "compact-thread"]);
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // push-task
    // ------------------------------------------------------------------

    #[test]
    fn parse_push_task_required_args() {
        let cli = parse(&[
            "agent-bus",
            "push-task",
            "--agent",
            "codex",
            "--task",
            r#"{"type":"review"}"#,
        ])
        .expect("push-task must parse");

        if let Cmd::PushTask { agent, task, .. } = cli.command {
            assert_eq!(agent, "codex");
            assert_eq!(task, r#"{"type":"review"}"#);
        } else {
            panic!("expected Cmd::PushTask");
        }
    }

    #[test]
    fn parse_push_task_missing_task_fails() {
        let result = parse(&["agent-bus", "push-task", "--agent", "codex"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_push_task_missing_agent_fails() {
        let result = parse(&["agent-bus", "push-task", "--task", "do something"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_push_task_defaults() {
        let cli = parse(&[
            "agent-bus",
            "push-task",
            "--agent",
            "codex",
            "--task",
            "review main.rs",
        ])
        .expect("push-task must parse");

        if let Cmd::PushTask {
            priority,
            created_by,
            repo,
            tags,
            depends_on,
            reply_to,
            ..
        } = cli.command
        {
            assert_eq!(priority, "normal");
            assert_eq!(created_by, "cli");
            assert!(repo.is_none());
            assert!(tags.is_empty());
            assert!(depends_on.is_empty());
            assert!(reply_to.is_none());
        } else {
            panic!("expected Cmd::PushTask");
        }
    }

    #[test]
    fn parse_push_task_with_card_fields() {
        let cli = parse(&[
            "agent-bus",
            "push-task",
            "--agent",
            "codex",
            "--task",
            "review main.rs",
            "--repo",
            "agent-bus",
            "--priority",
            "high",
            "--tags",
            "urgent,repo:agent-bus",
            "--depends-on",
            "task-001,task-002",
            "--reply-to",
            "msg-42",
            "--created-by",
            "claude",
        ])
        .expect("push-task with card fields must parse");

        if let Cmd::PushTask {
            agent,
            task,
            repo,
            priority,
            tags,
            depends_on,
            reply_to,
            created_by,
            ..
        } = cli.command
        {
            assert_eq!(agent, "codex");
            assert_eq!(task, "review main.rs");
            assert_eq!(repo.as_deref(), Some("agent-bus"));
            assert_eq!(priority, "high");
            assert_eq!(tags, vec!["urgent", "repo:agent-bus"]);
            assert_eq!(depends_on, vec!["task-001", "task-002"]);
            assert_eq!(reply_to.as_deref(), Some("msg-42"));
            assert_eq!(created_by, "claude");
        } else {
            panic!("expected Cmd::PushTask");
        }
    }

    // ------------------------------------------------------------------
    // pull-task
    // ------------------------------------------------------------------

    #[test]
    fn parse_pull_task_required_agent() {
        let cli =
            parse(&["agent-bus", "pull-task", "--agent", "euler"]).expect("pull-task must parse");

        if let Cmd::PullTask { agent, .. } = cli.command {
            assert_eq!(agent, "euler");
        } else {
            panic!("expected Cmd::PullTask");
        }
    }

    #[test]
    fn parse_pull_task_missing_agent_fails() {
        let result = parse(&["agent-bus", "pull-task"]);
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // peek-tasks
    // ------------------------------------------------------------------

    #[test]
    fn parse_peek_tasks_defaults() {
        let cli = parse(&["agent-bus", "peek-tasks", "--agent", "claude"])
            .expect("peek-tasks must parse");

        if let Cmd::PeekTasks { agent, limit, .. } = cli.command {
            assert_eq!(agent, "claude");
            assert_eq!(limit, 10); // default
        } else {
            panic!("expected Cmd::PeekTasks");
        }
    }

    #[test]
    fn parse_peek_tasks_custom_limit() {
        let cli = parse(&[
            "agent-bus",
            "peek-tasks",
            "--agent",
            "claude",
            "--limit",
            "25",
        ])
        .expect("peek-tasks with custom limit must parse");

        if let Cmd::PeekTasks { limit, .. } = cli.command {
            assert_eq!(limit, 25);
        } else {
            panic!("expected Cmd::PeekTasks");
        }
    }

    #[test]
    fn parse_peek_tasks_zero_limit() {
        // limit = 0 is the "return all" sentinel
        let cli = parse(&[
            "agent-bus",
            "peek-tasks",
            "--agent",
            "claude",
            "--limit",
            "0",
        ])
        .expect("peek-tasks --limit 0 must parse");

        if let Cmd::PeekTasks { limit, .. } = cli.command {
            assert_eq!(limit, 0);
        } else {
            panic!("expected Cmd::PeekTasks");
        }
    }

    // ------------------------------------------------------------------
    // resolve (positional resource arg)
    // ------------------------------------------------------------------

    #[test]
    fn parse_resolve_positional_resource() {
        let cli = parse(&[
            "agent-bus",
            "resolve",
            "src/redis_bus.rs",
            "--winner",
            "claude",
        ])
        .expect("resolve with positional resource must parse");

        if let Cmd::Resolve {
            resource, winner, ..
        } = cli.command
        {
            assert_eq!(resource, "src/redis_bus.rs");
            assert_eq!(winner, "claude");
        } else {
            panic!("expected Cmd::Resolve");
        }
    }

    #[test]
    fn parse_knock_defaults() {
        let cli = parse(&[
            "agent-bus",
            "knock",
            "--from-agent",
            "codex",
            "--to-agent",
            "claude",
        ])
        .expect("knock must parse");

        if let Cmd::Knock {
            from_agent,
            to_agent,
            body,
            request_ack,
            ..
        } = cli.command
        {
            assert_eq!(from_agent, "codex");
            assert_eq!(to_agent, "claude");
            assert_eq!(body, "check the bus");
            assert!(request_ack);
        } else {
            panic!("expected Cmd::Knock");
        }
    }

    // ------------------------------------------------------------------
    // watch
    // ------------------------------------------------------------------

    #[test]
    fn parse_watch_required_agent() {
        let cli = parse(&["agent-bus", "watch", "--agent", "gemini"]).expect("watch must parse");
        if let Cmd::Watch {
            agent,
            history,
            exclude_broadcast,
            ..
        } = cli.command
        {
            assert_eq!(agent, "gemini");
            assert_eq!(history, 0);
            assert!(!exclude_broadcast);
        } else {
            panic!("expected Cmd::Watch");
        }
    }

    // ------------------------------------------------------------------
    // presence
    // ------------------------------------------------------------------

    #[test]
    fn parse_presence_with_multiple_capabilities() {
        let cli = parse(&[
            "agent-bus",
            "presence",
            "--agent",
            "euler",
            "--capability",
            "mcp",
            "--capability",
            "rust",
            "--status",
            "busy",
        ])
        .expect("presence must parse");

        if let Cmd::Presence {
            agent,
            capability,
            status,
            ..
        } = cli.command
        {
            assert_eq!(agent, "euler");
            assert_eq!(status, "busy");
            assert_eq!(capability, vec!["mcp", "rust"]);
        } else {
            panic!("expected Cmd::Presence");
        }
    }

    // ------------------------------------------------------------------
    // inventory
    // ------------------------------------------------------------------

    #[test]
    fn parse_inventory_minimal() {
        let cli = parse(&["agent-bus", "inventory"]).expect("inventory must parse");
        if let Cmd::Inventory { repo, .. } = cli.command {
            assert!(repo.is_none());
        } else {
            panic!("expected Cmd::Inventory");
        }
    }

    #[test]
    fn parse_inventory_with_repo() {
        let cli = parse(&["agent-bus", "inventory", "--repo", "agent-bus"])
            .expect("inventory --repo must parse");
        if let Cmd::Inventory { repo, .. } = cli.command {
            assert_eq!(repo.as_deref(), Some("agent-bus"));
        } else {
            panic!("expected Cmd::Inventory");
        }
    }

    #[test]
    fn parse_inventory_with_encoding() {
        let cli = parse(&["agent-bus", "inventory", "--encoding", "json"])
            .expect("inventory --encoding must parse");
        assert!(matches!(cli.command, Cmd::Inventory { .. }));
    }

    // ------------------------------------------------------------------
    // subscribe
    // ------------------------------------------------------------------

    #[test]
    fn parse_subscribe_minimal() {
        let cli =
            parse(&["agent-bus", "subscribe", "--agent", "claude"]).expect("subscribe must parse");

        if let Cmd::Subscribe {
            agent,
            repo,
            session,
            thread,
            tag,
            topic,
            priority_min,
            resource,
            ttl,
            ..
        } = cli.command
        {
            assert_eq!(agent, "claude");
            assert!(repo.is_empty());
            assert!(session.is_empty());
            assert!(thread.is_empty());
            assert!(tag.is_empty());
            assert!(topic.is_empty());
            assert!(priority_min.is_none());
            assert!(resource.is_empty());
            assert!(ttl.is_none());
        } else {
            panic!("expected Cmd::Subscribe");
        }
    }

    #[test]
    fn parse_subscribe_with_all_scopes() {
        let cli = parse(&[
            "agent-bus",
            "subscribe",
            "--agent",
            "claude",
            "--repo",
            "agent-bus,other-repo",
            "--session",
            "s1",
            "--thread",
            "t1,t2",
            "--tag",
            "urgent",
            "--topic",
            "status,review-findings",
            "--priority-min",
            "high",
            "--resource",
            "src/main.rs",
            "--ttl",
            "3600",
        ])
        .expect("subscribe with all scopes must parse");

        if let Cmd::Subscribe {
            agent,
            repo,
            session,
            thread,
            tag,
            topic,
            priority_min,
            resource,
            ttl,
            ..
        } = cli.command
        {
            assert_eq!(agent, "claude");
            assert_eq!(repo, vec!["agent-bus", "other-repo"]);
            assert_eq!(session, vec!["s1"]);
            assert_eq!(thread, vec!["t1", "t2"]);
            assert_eq!(tag, vec!["urgent"]);
            assert_eq!(topic, vec!["status", "review-findings"]);
            assert_eq!(priority_min.as_deref(), Some("high"));
            assert_eq!(resource, vec!["src/main.rs"]);
            assert_eq!(ttl, Some(3600));
        } else {
            panic!("expected Cmd::Subscribe");
        }
    }

    #[test]
    fn parse_subscribe_missing_agent_fails() {
        let result = parse(&["agent-bus", "subscribe", "--repo", "agent-bus"]);
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // unsubscribe
    // ------------------------------------------------------------------

    #[test]
    fn parse_unsubscribe() {
        let cli = parse(&[
            "agent-bus",
            "unsubscribe",
            "--agent",
            "claude",
            "--id",
            "sub-uuid-123",
        ])
        .expect("unsubscribe must parse");

        if let Cmd::Unsubscribe { agent, id, .. } = cli.command {
            assert_eq!(agent, "claude");
            assert_eq!(id, "sub-uuid-123");
        } else {
            panic!("expected Cmd::Unsubscribe");
        }
    }

    #[test]
    fn parse_unsubscribe_missing_id_fails() {
        let result = parse(&["agent-bus", "unsubscribe", "--agent", "claude"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_unsubscribe_missing_agent_fails() {
        let result = parse(&["agent-bus", "unsubscribe", "--id", "sub-123"]);
        assert!(result.is_err());
    }

    // ------------------------------------------------------------------
    // subscriptions
    // ------------------------------------------------------------------

    #[test]
    fn parse_subscriptions() {
        let cli = parse(&["agent-bus", "subscriptions", "--agent", "claude"])
            .expect("subscriptions must parse");

        if let Cmd::Subscriptions { agent, .. } = cli.command {
            assert_eq!(agent, "claude");
        } else {
            panic!("expected Cmd::Subscriptions");
        }
    }

    #[test]
    fn parse_subscriptions_missing_agent_fails() {
        let result = parse(&["agent-bus", "subscriptions"]);
        assert!(result.is_err());
    }
}
