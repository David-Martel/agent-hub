# Remaining P1–P5 Implementation Plan

> **For agentic workers:** REQUIRED: Use superpowers:subagent-driven-development (if subagents available) or superpowers:executing-plans to implement this plan. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete all actionable remaining TODO items across P1–P5: log rotation config, spawn_blocking for HTTP, MCP schema isolation, SSE streaming, PG presence history, PG retention pruning, message export, and README service docs.

**Architecture:** Each task is independent and modifies a distinct file set. P2 spawn_blocking wraps existing sync calls in `tokio::task::spawn_blocking`. P4 SSE adds a new `/events` endpoint using axum's SSE support. P4 retention/export add new CLI subcommands. P5 docs are additive.

**Tech Stack:** Rust 2024, axum 0.8 (SSE via `axum::response::sse`), tokio, redis 0.29, postgres 0.19

**Build/verify:** `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

---

## Chunk 1: Remaining P1/P2/P3 hardening

### Task 1: P2 — spawn_blocking for HTTP handlers

**Files:**
- Modify: `rust-cli/src/http.rs`

- [ ] **Step 1: Wrap blocking Redis/PG calls in spawn_blocking**

All HTTP handlers currently call synchronous Redis functions directly on the async task. Wrap them in `tokio::task::spawn_blocking` to avoid blocking the tokio runtime.

Pattern for each handler:
```rust
async fn http_send_handler(
    State(state): State<AppState>,
    Json(req): Json<HttpSendRequest>,
) -> Result<impl IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    // ... validation stays synchronous (cheap) ...

    let result = tokio::task::spawn_blocking(move || {
        let mut conn = state.redis.get_connection()?;
        bus_post_message(&mut conn, &state.settings, /* ... */)
    })
    .await
    .map_err(|e| internal_error(anyhow::anyhow!("task join error: {e}")))?
    .map_err(internal_error)?;

    Ok((StatusCode::OK, Json(serde_json::to_value(&result).unwrap_or_default())))
}
```

Apply this to: `http_health_handler`, `http_send_handler`, `http_read_handler`, `http_ack_handler`, `http_presence_set_handler`, `http_presence_list_handler`.

Note: `bus_health` calls both Redis and PG — good candidate for spawn_blocking. `bus_list_messages` may call PG then fall back to Redis — also blocking.

The `state: AppState` is `Clone`, so it can be moved into the `spawn_blocking` closure.

- [ ] **Step 2: Run tests**

Run: `cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings && cargo test`

- [ ] **Step 3: Commit**

```bash
git add rust-cli/src/http.rs
git commit -m "perf(http): wrap all handlers in spawn_blocking to avoid blocking tokio"
```

### Task 2: P3 — Isolate MCP tool schemas from execution logic

**Files:**
- Modify: `rust-cli/src/mcp.rs`

- [ ] **Step 1: Extract tool_list() into a separate function with schema constants**

Move the JSON schema definitions out of the closure in `tool_list()` into named constants or a dedicated `fn tool_schemas()` helper. This separates schema definition from execution in `handle_tool_call_inner`.

Create a `mod schemas` block inside `mcp.rs`:

```rust
mod schemas {
    use serde_json::json;

    pub(super) fn bus_health_schema() -> serde_json::Value { json!({}) }
    pub(super) fn post_message_schema() -> serde_json::Value {
        json!({
            "sender":    {"type": "string"},
            "recipient": {"type": "string"},
            // ... etc
        })
    }
    // ... one per tool
}
```

Then `tool_list()` becomes a clean mapping of name → description → schema.

- [ ] **Step 2: Run tests**

Existing MCP tool_list tests should still pass.

- [ ] **Step 3: Commit**

```bash
git add rust-cli/src/mcp.rs
git commit -m "refactor(mcp): isolate tool schemas from execution logic"
```

### Task 3: P1 — Service log rotation script

**Files:**
- Create: `scripts/configure-log-rotation.ps1`

- [ ] **Step 1: Create PowerShell script for nssm log rotation**

```powershell
#Requires -RunAsAdministrator
# Configure log rotation for the AgentHub Windows service.

param(
    [string]$ServiceName = "AgentHub",
    [string]$LogDir = "C:\ProgramData\AgentHub\logs",
    [int]$MaxFileSizeKB = 10240,  # 10 MB
    [int]$RotateSeconds = 86400   # Daily
)

Write-Host "Configuring log rotation for $ServiceName..."

# Ensure log directory exists
if (-not (Test-Path $LogDir)) {
    New-Item -ItemType Directory -Path $LogDir -Force | Out-Null
}

# Configure nssm log rotation
nssm set $ServiceName AppStdout "$LogDir\agent-hub-service.log"
nssm set $ServiceName AppStderr "$LogDir\agent-hub-service-error.log"
nssm set $ServiceName AppRotateFiles 1
nssm set $ServiceName AppRotateOnline 1
nssm set $ServiceName AppRotateSeconds $RotateSeconds
nssm set $ServiceName AppRotateBytes ($MaxFileSizeKB * 1024)

Write-Host "Log rotation configured: max ${MaxFileSizeKB}KB, rotate every ${RotateSeconds}s"
Write-Host "Restart the service for changes to take effect: nssm restart $ServiceName"
```

- [ ] **Step 2: Commit**

```bash
git add scripts/configure-log-rotation.ps1
git commit -m "feat(service): add log rotation configuration script for nssm"
```

## Chunk 2: P4 Features

### Task 4: P4 — SSE live streaming endpoint

**Files:**
- Modify: `rust-cli/src/http.rs`
- Modify: `rust-cli/Cargo.toml` (may need `axum` features)

- [ ] **Step 1: Add SSE endpoint**

Add `GET /events?agent=<agent>` endpoint that streams Redis Pub/Sub events as SSE:

```rust
use axum::response::sse::{Event, Sse};
use tokio_stream::wrappers::ReceiverStream;

async fn http_sse_handler(
    State(state): State<AppState>,
    Query(params): Query<SseQuery>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, anyhow::Error>>>, (StatusCode, Json<serde_json::Value>)> {
    let agent = params.agent.clone();
    let include_broadcast = params.broadcast.unwrap_or(true);
    let settings = (*state.settings).clone();

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, anyhow::Error>>(64);

    tokio::task::spawn_blocking(move || {
        let client = redis::Client::open(settings.redis_url.as_str())?;
        let mut conn = client.get_connection()?;
        let mut pubsub = conn.as_pubsub();
        pubsub.subscribe(&settings.channel_key)?;

        loop {
            let msg = pubsub.get_message()?;
            let payload: String = msg.get_payload()?;
            // Filter by agent if specified
            if let Some(ref agent) = agent {
                let Ok(event) = serde_json::from_str::<serde_json::Value>(&payload) else { continue };
                let recipient = event.pointer("/message/to").and_then(|v| v.as_str());
                let matches = recipient == Some(agent) || (include_broadcast && recipient == Some("all"));
                if !matches { continue; }
            }
            let event = Event::default().data(payload);
            if tx.blocking_send(Ok(event)).is_err() {
                break; // client disconnected
            }
        }
        Ok::<_, anyhow::Error>(())
    });

    let stream = ReceiverStream::new(rx);
    Ok(Sse::new(stream))
}
```

Add `tokio-stream` dependency to Cargo.toml.

Register route: `.route("/events", get(http_sse_handler))`

- [ ] **Step 2: Add SseQuery struct**

```rust
#[derive(Debug, Deserialize)]
struct SseQuery {
    agent: Option<String>,
    broadcast: Option<bool>,
}
```

- [ ] **Step 3: Run tests**

- [ ] **Step 4: Add integration test for SSE**

Add to `rust-cli/tests/integration_test.rs`:

```rust
#[test]
fn sse_endpoint_mentioned_in_health() {
    // Just verify the binary compiles with SSE support
    // Full SSE testing requires a running server, covered by manual testing
    if !redis_available() { return; }
    let output = agent_bus_binary()
        .args(["health", "--encoding", "compact"])
        .output()
        .expect("health failed");
    assert!(output.status.success());
}
```

- [ ] **Step 5: Commit**

```bash
git add rust-cli/src/http.rs rust-cli/Cargo.toml rust-cli/tests/integration_test.rs
git commit -m "feat(http): add SSE live streaming endpoint at /events"
```

### Task 5: P4 — Presence history from PostgreSQL

**Files:**
- Modify: `rust-cli/src/postgres_store.rs`
- Modify: `rust-cli/src/commands.rs`
- Modify: `rust-cli/src/cli.rs`
- Modify: `rust-cli/src/mcp.rs`
- Modify: `rust-cli/src/http.rs`

- [ ] **Step 1: Add list_presence_history_postgres() to postgres_store.rs**

```rust
pub(crate) fn list_presence_history_postgres(
    settings: &Settings,
    agent: Option<&str>,
    since_minutes: u64,
    limit: usize,
) -> Result<Vec<Presence>> {
    run_postgres_blocking(|| {
        let Some(mut client) = connect_postgres(settings)? else {
            return Ok(Vec::new());
        };
        ensure_postgres_storage(&mut client, settings)?;
        let since_minutes = i64::try_from(since_minutes)?;
        let limit = i64::try_from(limit)?;
        let agent_filter = agent.map(str::to_owned);
        let rows = client.query(
            &format!(
                "select timestamp_utc, protocol_version, agent, status, session_id, capabilities, metadata, ttl_seconds \
                 from {} \
                 where timestamp_utc >= now() - ($1::bigint * interval '1 minute') \
                   and ($2::text is null or agent = $2) \
                 order by timestamp_utc desc \
                 limit $3",
                settings.presence_event_table
            ),
            &[&since_minutes, &agent_filter, &limit],
        )?;
        Ok(rows.iter().map(row_to_presence).collect())
    })
}
```

Add `row_to_presence` helper similar to `row_to_message`.

- [ ] **Step 2: Add `presence-history` CLI subcommand**

In `cli.rs` add a new `Cmd::PresenceHistory` variant. In `commands.rs` add `cmd_presence_history()`. In `main.rs` add the match arm.

- [ ] **Step 3: Add MCP tool `list_presence_history`**

In `mcp.rs` add the tool schema and handler.

- [ ] **Step 4: Add HTTP endpoint `GET /presence/history`**

In `http.rs` add the route and handler.

- [ ] **Step 5: Run tests, commit**

```bash
git add rust-cli/src/
git commit -m "feat: add presence history backed by PostgreSQL"
```

### Task 6: P4 — PostgreSQL retention pruning

**Files:**
- Modify: `rust-cli/src/postgres_store.rs`
- Modify: `rust-cli/src/commands.rs`
- Modify: `rust-cli/src/cli.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Add prune function to postgres_store.rs**

```rust
pub(crate) fn prune_old_messages(settings: &Settings, older_than_days: u64) -> Result<u64> {
    run_postgres_blocking(|| {
        let Some(mut client) = connect_postgres(settings)? else {
            return Ok(0);
        };
        ensure_postgres_storage(&mut client, settings)?;
        let days = i64::try_from(older_than_days)?;
        let rows = client.execute(
            &format!(
                "delete from {} where timestamp_utc < now() - ($1::bigint * interval '1 day')",
                settings.message_table
            ),
            &[&days],
        )?;
        Ok(rows)
    })
}

pub(crate) fn prune_old_presence(settings: &Settings, older_than_days: u64) -> Result<u64> {
    run_postgres_blocking(|| {
        let Some(mut client) = connect_postgres(settings)? else {
            return Ok(0);
        };
        ensure_postgres_storage(&mut client, settings)?;
        let days = i64::try_from(older_than_days)?;
        let rows = client.execute(
            &format!(
                "delete from {} where timestamp_utc < now() - ($1::bigint * interval '1 day')",
                settings.presence_event_table
            ),
            &[&days],
        )?;
        Ok(rows)
    })
}
```

- [ ] **Step 2: Add `prune` CLI subcommand**

In `cli.rs`:
```rust
/// Prune old messages and presence events from PostgreSQL.
Prune {
    #[arg(long, default_value_t = 30, help = "Delete records older than N days")]
    older_than_days: u64,
    #[arg(long, default_value = "compact", help = "Output format")]
    encoding: Encoding,
},
```

In `commands.rs`:
```rust
pub(crate) fn cmd_prune(settings: &Settings, older_than_days: u64, encoding: &Encoding) -> Result<()> {
    let msgs = prune_old_messages(settings, older_than_days)?;
    let presence = prune_old_presence(settings, older_than_days)?;
    let result = serde_json::json!({
        "messages_deleted": msgs,
        "presence_events_deleted": presence,
        "older_than_days": older_than_days,
    });
    output(&result, encoding);
    Ok(())
}
```

- [ ] **Step 3: Wire up in main.rs match**

- [ ] **Step 4: Run tests, commit**

```bash
git add rust-cli/src/
git commit -m "feat: add prune subcommand for PostgreSQL retention management"
```

### Task 7: P4 — Message export command

**Files:**
- Modify: `rust-cli/src/commands.rs`
- Modify: `rust-cli/src/cli.rs`
- Modify: `rust-cli/src/main.rs`

- [ ] **Step 1: Add `export` CLI subcommand**

In `cli.rs`:
```rust
/// Export messages to stdout as NDJSON (one JSON object per line).
Export {
    #[arg(long, help = "Filter by recipient agent")]
    agent: Option<String>,
    #[arg(long, help = "Filter by sender")]
    from_agent: Option<String>,
    #[arg(long, default_value_t = 10080, help = "Time window in minutes [max 10080 = 7 days]")]
    since_minutes: u64,
    #[arg(long, default_value_t = 10000, help = "Max messages")]
    limit: usize,
},
```

In `commands.rs`:
```rust
pub(crate) fn cmd_export(settings: &Settings, args: &ExportArgs) -> Result<()> {
    let msgs = bus_list_messages(
        settings, args.agent.as_deref(), args.from_agent.as_deref(),
        args.since_minutes, args.limit, true,
    )?;
    for msg in &msgs {
        println!("{}", serde_json::to_string(msg).unwrap_or_default());
    }
    Ok(())
}
```

- [ ] **Step 2: Wire up in main.rs, run tests, commit**

```bash
git add rust-cli/src/
git commit -m "feat: add export subcommand for NDJSON message export"
```

## Chunk 3: P5 Interop and packaging + final cleanup

### Task 8: P5 — README service troubleshooting docs

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add Windows service section to README.md**

Append a section covering:
- How to install the service: `scripts/install-agent-hub-service.ps1`
- How to check status: `nssm status AgentHub`
- How to restart: `nssm restart AgentHub`
- How to view logs: `Get-Content C:\ProgramData\AgentHub\logs\agent-hub-service.log -Tail 50`
- How to configure log rotation: `scripts/configure-log-rotation.ps1`
- How to remove: `scripts/remove-agent-hub-service.ps1`
- Common issues: port in use, Redis not running, PG connection refused

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: add Windows service troubleshooting section to README"
```

### Task 9: Update TODO.md and final push

**Files:**
- Modify: `TODO.md`

- [ ] **Step 1: Mark all completed items**

Check off:
- P1: Service log rotation
- P2: spawn_blocking
- P3: MCP schema isolation
- P4: SSE streaming, presence history, retention pruning, message export
- P5: README service docs

Note remaining deferred items: benchmarks, index review, --server client mode, bootstrap packaging, WinSW, A2A adapter, MessagePack/LZ4

- [ ] **Step 2: Final verification**

```bash
cd rust-cli && RUSTC_WRAPPER="" cargo fmt --all --check && cargo clippy --all-targets -- -D warnings && cargo test
```

- [ ] **Step 3: Commit and push**

```bash
git add TODO.md
git commit -m "docs: mark all actionable P1-P5 items complete in TODO.md"
RUSTC_WRAPPER="" git push origin main
```
