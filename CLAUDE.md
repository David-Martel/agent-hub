# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Redis + PostgreSQL agent coordination bus for multi-agent systems (Claude, Codex, Gemini). The primary implementation is a **Rust standalone binary** (`rust-cli/`) providing CLI, MCP server (stdio), and HTTP REST server. Storage is dual: Redis (realtime streams + pub/sub) with PostgreSQL (durable history, tag-indexed queries).

**Config**: Settings load from `~/.config/agent-bus/config.json` → env vars → hardcoded defaults.

## Build & Development

```bash
# Build release (binary deploys to ~/bin/agent-bus.exe)
cd rust-cli && RUSTC_WRAPPER="" cargo build --release

# Clippy (required — pre-push hook enforces)
cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings

# Tests (149 total: 145 unit + 4 integration)
cd rust-cli && RUSTC_WRAPPER="" cargo test

# Format
cd rust-cli && cargo fmt --all --check
```

**sccache note**: If builds fail with `SCCACHE_SERVER_PORT` errors, set `RUSTC_WRAPPER=""`.

## Architecture

The Rust implementation (`rust-cli/src/`, ~10000 LOC) is split into 17 modules:

| Module | Purpose |
|--------|---------|
| `main.rs` | Crate root, startup, `main()` dispatch, `OnceLock<PgWriter>` |
| `settings.rs` | Config file + env loading, `Settings::validate()`, `redact_url()` |
| `models.rs` | `Message`, `Presence`, `Health` structs, protocol constants |
| `redis_bus.rs` | Redis r2d2 pool, stream ops, pub/sub, presence, health, LZ4 compression |
| `postgres_store.rs` | PG connect, persist with retry + circuit breaker, `PgWriter` async mpsc |
| `output.rs` | `Encoding` enum (json/compact/human/toon), formatters, `minimize_value()` |
| `validation.rs` | Priority/field validation, message schema validation (`finding`/`status`/`benchmark`) |
| `cli.rs` | Clap parser with 16+ subcommands |
| `commands.rs` | CLI command implementations |
| `mcp.rs` | `AgentBusMcpServer` with `schemas` submodule, MCP Streamable HTTP |
| `http.rs` | Axum REST + SSE streaming + batch endpoints + channel routes |
| `journal.rs` | Per-repo NDJSON export with idempotent cursor tracking |
| `monitor.rs` | Real-time session monitoring dashboard with agent status |
| `mcp_discovery.rs` | Auto-discover MCP tools from `.claude/mcp.json` for preambles |
| `channels.rs` | Structured comms: direct, group, escalate, arbitrate channels |
| `codex_bridge.rs` | Codex CLI integration: config discovery, finding sync, formatting |

### Transport Modes

- **MCP stdio** (`serve --transport stdio`): For LLM tool integration (Claude/Codex/Gemini MCP configs)
- **MCP Streamable HTTP** (`serve --transport mcp-http --port 8401`): MCP 2025-06-18 spec transport
- **HTTP REST** (`serve --transport http --port 8400`): Server mode with SSE streaming at `/events`
- **CLI**: Direct Redis/PG access for scripting and agent subprocesses

### CLI Subcommands

`health`, `send`, `read`, `watch`, `ack`, `presence`, `presence-list`, `serve`, `prune`, `export`, `presence-history`, `journal`, `sync`, `monitor`, `batch-send`, `pending-acks`, `claim`, `claims`, `resolve`, `codex`

### MCP Tool Names (for stdio and mcp-http transports)

Exposes 7+ tools: `bus_health`, `post_message`, `list_messages`, `ack_message`, `set_presence`, `list_presence`, `list_presence_history`, plus channel tools: `create_channel`, `post_to_channel`, `read_channel`, `claim_resource`, `resolve_claim`. See [`AGENT_COMMUNICATIONS.md`](AGENT_COMMUNICATIONS.md) for full parameter docs.

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
| `RUST_LOG` | `error` | env only |

## MCP Platform Configs

All 3 platforms configured identically at:
- Claude Code: `~/.claude/mcp.json` (key: `agent-bus`)
- Codex: `~/.codex/config.toml` (key: `agent_bus`)
- Gemini: `~/.gemini/settings.json` (key: `agent-bus`)

## Git Hooks

Lefthook pre-push: `rust-clippy` + `rust-audit` (both blocking).

## Rust Conventions

- **Allocator**: mimalloc per M-MIMALLOC-APPS
- **Error handling**: `anyhow::Result` with `.context()` — no unwrap in business logic
- **Lints**: Clippy pedantic + restriction subset in `Cargo.toml [lints]`
- **Edition**: 2024
