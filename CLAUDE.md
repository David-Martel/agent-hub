# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Redis + PostgreSQL agent coordination bus for multi-agent systems (Claude,
Codex, Gemini). The primary implementation is a Rust package in `rust-cli/`
with a shared library runtime and three current binary faces:

- `agent-bus.exe`
- `agent-bus-http.exe`
- `agent-bus-mcp.exe`

Storage is dual: Redis (realtime streams + pub/sub) with PostgreSQL (durable
history, tag-indexed queries).

**Config**: Settings load from `~/.config/agent-bus/config.json` → env vars → hardcoded defaults.

## Build & Development

Canonical structural refactor plan:
- [`agents.TODO.md`](./agents.TODO.md)

```bash
# Preferred repo-root local build/test entrypoint
pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease

# Release/profile builds for all current binary surfaces
cd rust-cli && cargo build --release --bins

# Clippy (required — pre-push hook enforces)
cd rust-cli && cargo clippy --all-targets -- -D warnings

# Tests
cd rust-cli && cargo test

# Benchmarks (criterion: token estimation, TOON, MessagePack)
cd rust-cli && cargo bench

# Format
cd rust-cli && cargo fmt --all --check

# Build + Deploy + Restart service (one command)
pwsh -NoLogo -NoProfile -File scripts/build-deploy.ps1
# Or skip build: scripts/build-deploy.ps1 -SkipBuild
```

**sccache note**: Repo-local cargo usage now goes through the checked-in
`.cargo/config.toml` wrapper and uses `sccache` automatically when present. If
you need to bypass that path for debugging, override `RUSTC_WRAPPER=""` in the
current shell.

## Architecture

The Rust implementation (`rust-cli/src/`, ~20400 LOC, 403 tests) is split into 20 modules:

| Module | Purpose |
|--------|---------|
| `lib.rs` | Crate root, `main_entry()` dispatch, `OnceLock<PgWriter>`, runtime setup |
| `main.rs` | Thin binary stub — delegates to `agent_bus::main_entry()` |
| `settings.rs` | Config file + env loading, `Settings::validate()`, `redact_url()` |
| `models.rs` | `Message`, `Presence`, `Health` structs, protocol constants |
| `redis_bus.rs` | Redis r2d2 pool, stream ops, pub/sub, presence, health, LZ4 compression |
| `postgres_store.rs` | PG connect, persist with retry + circuit breaker, `PgWriter` async mpsc |
| `output.rs` | `Encoding` enum (json/compact/human/toon), formatters, `minimize_value()` |
| `validation.rs` | Priority/field validation, message schema validation (`finding`/`status`/`benchmark`) |
| `ops.rs` | Shared bus operations (send/ack/knock/presence) reused across CLI, HTTP, MCP |
| `cli.rs` | Clap parser with 35 subcommands |
| `commands.rs` | CLI command implementations |
| `mcp.rs` | `AgentBusMcpServer` with `schemas` submodule, MCP Streamable HTTP |
| `http.rs` | Axum REST + SSE streaming + batch endpoints + channel routes |
| `journal.rs` | Per-repo NDJSON export with idempotent cursor tracking |
| `monitor.rs` | Real-time session monitoring dashboard with agent status |
| `mcp_discovery.rs` | Auto-discover MCP tools from Claude MCP config files for preambles |
| `channels.rs` | Structured comms: direct, group, escalate, arbitrate channels |
| `codex_bridge.rs` | Codex CLI integration: config discovery, finding sync, formatting |
| `server_mode.rs` | Shared HTTP client helpers for CLI server-mode routing |
| `token.rs` | Token estimation, message minimization, LLM context compaction |

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

## Git Hooks

Lefthook pre-push: `rust-clippy` + `rust-audit` (both blocking).

## Rust Conventions

- **Allocator**: mimalloc per M-MIMALLOC-APPS
- **Error handling**: `anyhow::Result` with `.context()` — no unwrap in business logic
- **Lints**: Clippy pedantic + restriction subset in `Cargo.toml [lints]`
- **Edition**: 2024
