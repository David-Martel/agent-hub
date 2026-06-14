# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Redis + PostgreSQL agent coordination bus for multi-agent systems (Claude,
Codex, Gemini). The workspace contains exactly four crates; `rust-cli` has
been removed.

Current crate layout:

- `crates/agent-bus-core` — shared library: storage adapters (redis_bus,
  postgres_store), channels, settings, models, token helpers, validation,
  output, journal, codex_bridge, agent_profile, bootstrap, mcp_dispatch
  (shared `McpToolDispatch`), error, and ops subtree.
- `crates/agent-bus-cli` — package name `agent-bus`, produces the `agent-bus`
  binary. Owns cli.rs, commands.rs, lib.rs, server_mode.rs, monitor.rs,
  mcp_discovery.rs. Still links axum/rmcp/reqwest because `serve` starts
  HTTP/MCP inline. Depends on `agent-bus-core`.
- `crates/agent-bus-http` — package name `agent-bus-http`, produces the
  `agent-bus-http` binary. Owns http.rs. Depends only on `agent-bus-core`
  plus Axum/rmcp/Redis.
- `crates/agent-bus-mcp` — package name `agent-bus-mcp`, produces the
  `agent-bus-mcp` binary. Owns mcp.rs. Lightest dependency footprint.

**Note on agent-bus-cli:** `lib.rs` still declares private shim modules
(`channels`, `redis_bus`, `postgres_store`, etc.) that re-export from
`agent-bus-core`. These are internal to the CLI crate and will be cleaned up
in a follow-on pass. The canonical implementations live in `agent-bus-core`.

Current binary surfaces:

- `agent-bus.exe` (from `crates/agent-bus-cli`)
- `agent-bus-http.exe` (from `crates/agent-bus-http`)
- `agent-bus-mcp.exe` (from `crates/agent-bus-mcp`)

Storage is dual: Redis (realtime streams + pub/sub) with PostgreSQL (durable
history, tag-indexed queries).

**Config**: Settings load from `~/.config/agent-bus/config.json` → env vars → hardcoded defaults.

## Build & Development

Structural refactor tracking (split complete; remaining cleanup):
- [`agents.TODO.md`](./agents.TODO.md)

Code-grounded status snapshot (post-split):
- [`docs/current-status-2026-06-13.md`](./docs/current-status-2026-06-13.md)

```bash
# Preferred repo-root local build/test entrypoint
pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease

# Workspace sanity check (unit tests, no external services needed)
cargo test --workspace --lib --bins

# Build all binaries, release profile
cargo build --workspace --release --bins

# Clippy (required — pre-push hook enforces)
cargo clippy --workspace --all-targets -- -D warnings

# Tests (unit only, no external services)
cargo test --workspace --lib --bins

# Integration tests (requires Redis :6380 + PostgreSQL :5300)
cargo test --workspace --test '*'

# Benchmarks (criterion: token estimation, TOON, MessagePack)
cargo bench --workspace

# Format check
cargo fmt --all --check

# Build + Deploy + Restart service (one command)
pwsh -NoLogo -NoProfile -File scripts/build-deploy.ps1
# Or skip build: scripts/build-deploy.ps1 -SkipBuild
```

**sccache note**: Repo-local cargo usage now goes through the checked-in
`.cargo/config.toml` wrapper and uses `sccache` automatically when present. If
you need to bypass that path for debugging, override `RUSTC_WRAPPER=""` in the
current shell.

## Architecture

Workspace layout (`rust-cli` has been removed):

- `crates/agent-bus-core`
  - shared library depended on by all three binary crates
  - storage adapters: `redis_bus`, `postgres_store`
  - channels, settings, models, token helpers, validation, output, journal
  - `codex_bridge.rs` — Codex-specific sync helpers
  - `agent_profile.rs` — `AgentProfile` trait
  - `bootstrap.rs` — `bootstrap()` + `BootstrapGuard` (PgWriter init, tracing, startup announce)
  - `mcp_dispatch.rs` — `McpToolDispatch`: shared MCP tool definitions + dispatch (no rmcp dependency)
  - `error.rs` — shared error types
  - `ops/` — typed ops subtree: ack_deadline, admin, channel, claim, inbox, inventory, message, subscription, task, thread
- `crates/agent-bus-cli` (package `agent-bus`, binary `agent-bus`)
  - owns `lib.rs`, `cli.rs`, `commands.rs`, `server_mode.rs`, `monitor.rs`, `mcp_discovery.rs`
  - entry point: `main.rs` → `main_entry()` in `lib.rs`
  - integration tests under `crates/agent-bus-cli/tests/`
  - Note: still carries private shim modules re-exporting from `agent-bus-core`
- `crates/agent-bus-http` (package `agent-bus-http`, binary `agent-bus-http`)
  - owns `http.rs` (Axum router, SSE, MCP-HTTP bridge via `McpToolDispatch`)
  - `main.rs` calls `bootstrap()` from core directly
- `crates/agent-bus-mcp` (package `agent-bus-mcp`, binary `agent-bus-mcp`)
  - owns `mcp.rs` (`AgentBusMcpServer`: rmcp `ServerHandler` wrapping `McpToolDispatch`)
  - `main.rs` calls `bootstrap()` from core directly

### Transport Modes

- **MCP stdio** (`serve --transport stdio`): For LLM tool integration (Claude/Codex/Gemini MCP configs)
- **MCP Streamable HTTP** (`serve --transport mcp-http --port 8401`): MCP 2025-06-18 spec transport
- **HTTP REST** (`serve --transport http --port 8400`): Server mode with SSE streaming at `/events`
- **CLI**: Direct Redis/PG access for scripting and agent subprocesses

### CLI Subcommands

`health`, `send`, `read`, `watch`, `ack`, `presence`, `presence-list`, `serve`, `prune`, `export`, `presence-history`, `journal`, `sync`, `monitor`, `batch-send`, `pending-acks`, `claim`, `renew-claim`, `release-claim`, `claims`, `resolve`, `knock`, `codex-sync`, `service`, `post-direct`, `read-direct`, `post-group`, `read-group`, `session-summary`, `dedup`, `token-count`, `compact-context`, `push-task`, `pull-task`, `peek-tasks`

### MCP Tool Names (17 tools for stdio and mcp-http transports)

`bus_health`, `post_message`, `list_messages`, `ack_message`, `set_presence`, `list_presence`, `list_presence_history`, `negotiate`, `create_channel`, `post_to_channel`, `read_channel`, `claim_resource`, `renew_claim`, `release_claim`, `resolve_claim`, `knock_agent`, `check_inbox`. See [`AGENT_COMMUNICATIONS.md`](AGENT_COMMUNICATIONS.md) for full parameter docs.

### Channel System (v0.4)

Structured communication beyond broadcast:
- **Direct**: `POST /channels/direct/:agent` — private agent-to-agent with delivery confirmation
- **Group**: `POST /channels/groups/:name/messages` — named group discussions with member lists
- **Escalate**: `POST /channels/escalate` — auto-routes to orchestrator, priority=high
- **Arbitrate**: `POST /channels/arbitrate/:resource` — ownership claims with priority arguments

### Token-Optimized Features

- **TOON encoding** (`--encoding toon`): 70% token savings vs JSON. Format: `@from→to #topic [tags] body`
- **LZ4 compression**: Bodies >512 bytes auto-compressed, transparent decompression on read
- **Batch operations**: `/messages/batch`, `/read/batch`, `/ack/batch` for bulk operations

### Message Schema Validation

Use `--schema finding|status|benchmark` on `send` to validate message structure:
- **finding**: Requires `FINDING:` + `SEVERITY:`, or `FIX`/`COMPLETE` keywords
- **status**: Non-empty body
- **benchmark**: Contains key=value or key:value metrics

### Storage

**Redis** (realtime, always required): Streams (MAXLEN~100000), Pub/Sub, Presence (TTL), AOF persistence
**PostgreSQL** (durable history): Auto-creates tables, GIN index on tags, circuit breaker (60s cooldown)

## Environment Variables

| Variable | Default | Source |
|----------|---------|--------|
| `AGENT_BUS_CONFIG` | `~/.config/agent-bus/config.json` | Config file path |
| `AGENT_BUS_REDIS_URL` | `redis://localhost:6380/0` | config.json |
| `AGENT_BUS_DATABASE_URL` | `postgresql://postgres@localhost:5300/redis_backend` | config.json |
| `AGENT_BUS_SERVER_HOST` | `localhost` | config.json |
| `AGENT_BUS_STREAM_MAXLEN` | `100000` | config.json |
| `AGENT_BUS_SESSION_ID` | (none) | env only — auto-tags messages with `session:<id>` |
| `RUST_LOG` | `error` | env only |

## MCP Platform Configs

All 3 platforms configured identically at:
- Claude Code: `~/.claude.json` top-level `mcpServers["agent-bus"]`
- Codex: `~/.codex/config.toml` (key: `agent_bus`)
- Gemini: `~/.gemini/settings.json` (key: `agent-bus`)

Example configs for all platforms live in `examples/mcp/`.

## Git Hooks

Lefthook (install with `lefthook install`):
- **pre-commit** (parallel): `cargo fmt --check`, `cargo clippy`, `ast-grep scan`
- **pre-push** (parallel): `cargo test`, `cargo audit`
- **commit-msg**: conventional commit format advisory

## Rust Conventions

- **Allocator**: mimalloc per M-MIMALLOC-APPS
- **Error handling**: `anyhow::Result` with `.context()` — no unwrap in business logic
- **Lints**: Clippy pedantic + restriction subset in workspace root `Cargo.toml [workspace.lints]`
- **Edition**: 2024
