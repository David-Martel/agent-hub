# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Redis-backed agent coordination bus for multi-agent systems (Claude, Codex, Gemini). The primary implementation is a **Rust standalone binary** (`rust-cli/`) that provides both a CLI and MCP server. Storage is dual: Redis (primary, realtime) with optional PostgreSQL persistence. A legacy Python package (`src/`) is retained only for the PyO3 codec extension and test suite.

## Build & Development

### Rust CLI (primary — all new work goes here)

```bash
# Build (from repo root)
cd rust-cli && cargo build --release
# The release binary is deployed to ~/bin/agent-bus.exe

# Clippy (required — pre-push hook enforces this)
cd rust-cli && RUSTC_WRAPPER="" cargo clippy --all-targets -- -D warnings

# Format check
cd rust-cli && cargo fmt --all --check
```

**sccache note**: If clippy/build fails with `SCCACHE_SERVER_PORT` errors, set `RUSTC_WRAPPER=""` to bypass sccache.

### Python tests (codec + bus integration)

```bash
uv run pytest --cov                    # all tests
uv run pytest tests/test_codec.py -v   # single module
uv run pytest -k "test_name"           # single test
```

### PyO3 codec extension

```bash
pwsh -NoLogo -NoProfile -File scripts/build-native-codec.ps1 -Release
```

## Architecture

The entire Rust implementation lives in a single file `rust-cli/src/main.rs` (~2700 LOC), organized into numbered sections:

| Section | Purpose |
|---------|---------|
| 1 – Settings | `Settings::from_env()` reads all `AGENT_BUS_*` environment variables |
| 2 – Models | `Message`, `Presence`, `Health` structs (serde) |
| 3 – Bus | Redis + PostgreSQL operations: connect, send, read, health, presence, pub/sub, persistent storage |
| 4 – Encoding | Four output modes: `json`, `compact`, `minimal`, `human` (table) |
| 5 – CLI | Clap parser with 8 subcommands |
| 6 – Validation | Input validation helpers |
| 7 – Commands | CLI command implementations |
| 8 – MCP server | `AgentBusMcpServer` implementing `rmcp::ServerHandler` |
| 8b – HTTP server | Axum REST routes mirroring all MCP tools |
| 9 – Startup | Startup announcement helper |
| 10 – main | Transport selection (MCP stdio vs HTTP) |
| 11 – Tests | Inline `#[cfg(test)]` unit tests |

### Dual Transport

- **MCP stdio** (`serve --transport stdio`): Default mode for LLM tool integration via rmcp
- **HTTP REST** (`serve --transport http --port 8400`): Axum server with CORS, same tool set as MCP

### Storage

**Redis** (primary, always required):
- **Streams** (`agent_bus:messages`): Durable message history, XADD with MAXLEN~5000, XREVRANGE with 5x overfetch for filtered queries
- **Pub/Sub** (`agent_bus:events`): Realtime watch notifications
- **Presence** (`agent_bus:presence:{agent}`): TTL-based agent registration via SET EX

**PostgreSQL** (optional, for long-term persistence):
- Auto-creates `messages` and `presence` tables if `AGENT_BUS_DATABASE_URL` is set
- Messages and presence records are written to both Redis and Postgres
- Blocking sync client via `run_postgres_blocking()` on a Tokio `spawn_blocking` thread

### Message Protocol (v1.0)

Required fields: `id` (UUID), `timestamp_utc`, `protocol_version`, `from`, `to`, `topic`, `body`. Optional: `thread_id`, `tags`, `priority` (low/normal/high/urgent), `request_ack`, `reply_to`, `metadata`.

## CLI Subcommands

`health`, `send`, `read`, `watch`, `ack`, `presence`, `presence-list`, `serve`

## Environment Variables

| Variable | Default |
|----------|---------|
| `AGENT_BUS_REDIS_URL` | `redis://localhost:6380/0` |
| `AGENT_BUS_STREAM_KEY` | `agent_bus:messages` |
| `AGENT_BUS_STREAM_MAXLEN` | `5000` |
| `AGENT_BUS_STARTUP_ENABLED` | `true` |
| `AGENT_BUS_DATABASE_URL` | *(unset — Postgres disabled)* |
| `RUST_LOG` | `error` |

## Git Hooks

Lefthook pre-push runs `rust-clippy` and `rust-audit` on Rust files. Clippy failures block push. If sccache port conflicts cause false failures, use `RUSTC_WRAPPER="" git push`.

## Rust Conventions

- **Allocator**: mimalloc (`#[global_allocator]`) per M-MIMALLOC-APPS
- **Error handling**: `anyhow::Result` with `.context()` — no unwrap in business logic
- **Lints**: Clippy pedantic + restriction subset enabled in `Cargo.toml [lints]`
- **Edition**: 2024
