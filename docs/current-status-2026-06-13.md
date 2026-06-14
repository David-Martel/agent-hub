# Current Status (2026-06-13)

This document is the code-grounded status snapshot for the repository as of
2026-06-13. It supersedes
[`docs/current-status-2026-04-03.md`](./current-status-2026-04-03.md), which
described the workspace mid-migration when `rust-cli` was still present.

## Executive Summary

- The `rust-cli` crate has been removed. The Cargo workspace now contains
  exactly four crates.
- `cargo check --workspace` is clean. 590+ library tests pass on Linux via
  Docker (CI green).
- Integration tests (47 HTTP, 6 channel, 5 CLI) live under
  `crates/agent-bus-cli/tests/` and are environment-dependent (skip when
  Redis/PG/HTTP service is not running).
- `agent-bus-cli` is the "fat CLI" crate: it still carries local shim modules
  that re-export from `agent-bus-core` (see caveat below).
- Transport surfaces (CLI + HTTP + MCP) are all functional: MCP stdio, MCP
  Streamable HTTP, HTTP/REST, and CLI direct mode.

## Confirmed Workspace Layout

Root `Cargo.toml` workspace members (confirmed from manifest):

```
[workspace]
members = [
    "crates/agent-bus-core",
    "crates/agent-bus-cli",
    "crates/agent-bus-http",
    "crates/agent-bus-mcp",
]
```

### `crates/agent-bus-core` (library, package name `agent-bus-core`)

Shared runtime library. All three binary crates depend on it. Owns:

- `agent_profile.rs` — `AgentProfile` trait
- `bootstrap.rs` — `bootstrap()` + `BootstrapGuard` (PgWriter init, tracing, startup announce)
- `channels.rs` — channel routing logic
- `codex_bridge.rs` — Codex-specific sync helpers
- `error.rs` — shared error types
- `journal.rs` — journal/compaction helpers
- `mcp_dispatch.rs` — `McpToolDispatch`: shared MCP tool definitions and dispatch (no rmcp dependency)
- `models.rs` — shared message types
- `ops/` — typed ops (ack_deadline, admin, channel, claim, inbox, inventory, message, subscription, task, thread)
- `output.rs` — encoding/formatting helpers
- `postgres_store.rs` — PostgreSQL adapter
- `redis_bus.rs` — Redis adapter
- `settings.rs` — `Settings` struct (config.json → env vars → defaults)
- `token.rs` — token estimation helpers
- `validation.rs` — message schema validation

The `cli` feature gates `clap::ValueEnum` on `Encoding` so `agent-bus-core`
can be depended on without pulling in clap.

### `crates/agent-bus-cli` (binary, package name `agent-bus`, binary `agent-bus`)

CLI entry point. Owns the full CLI/HTTP/MCP transport surface because the
`serve` subcommand can start HTTP or MCP inline. Source files:

- `cli.rs` — Clap CLI definition (`Cli`, `Cmd`)
- `commands.rs` — command implementations
- `lib.rs` — `main_entry()`, `main_entry_with_args()`, dispatch via `Cmd`
- `main.rs` — thin `fn main()` wrapper calling `main_entry()`
- `server_mode.rs` — server-mode client helpers (HTTP client for `AGENT_BUS_SERVER_URL`)
- `monitor.rs` — watch/monitor helpers
- `mcp_discovery.rs` — MCP capability negotiation and tool schema advertisement

**Caveat — still fat**: `lib.rs` declares local private modules (`channels`,
`codex_bridge`, `journal`, `models`, `ops`, `output`, `postgres_store`,
`redis_bus`, `settings`, `token`, `validation`) that are parallel shim files
re-exporting from `agent-bus-core`. These exist for historical compatibility
within the CLI crate and will be cleaned up in a follow-on pass. The canonical
implementations live in `agent-bus-core`.

The `server-mode` feature (on by default) gates `reqwest` for server-mode
client paths. Dependency graph includes `axum`, `rmcp`, and `reqwest` because
`serve --transport http` and `serve --transport stdio` start those servers inline.

### `crates/agent-bus-http` (binary, package name `agent-bus-http`, binary `agent-bus-http`)

HTTP/SSE server. Owns:

- `main.rs` — `fn main()` → spawns thread → Tokio runtime → `run()` →
  `bootstrap()` + `http::start_http_server()`
- `http.rs` — Axum router, all HTTP routes, SSE fanout, MCP-HTTP bridge via
  `McpToolDispatch` from `agent-bus-core`

Depends only on `agent-bus-core` plus Axum/rmcp/Redis/Tokio.
Does NOT depend on `agent-bus-cli`.

### `crates/agent-bus-mcp` (binary, package name `agent-bus-mcp`, binary `agent-bus-mcp`)

MCP stdio server. Owns:

- `main.rs` — `fn main()` → spawns thread → Tokio runtime → `run()` →
  `bootstrap()` + `serve_server(AgentBusMcpServer, stdio)`
- `mcp.rs` — `AgentBusMcpServer`: rmcp `ServerHandler` wrapper around
  `McpToolDispatch`

Depends only on `agent-bus-core` plus `rmcp`/`serde_json`/Tokio. Minimal
dependency footprint.

## Transport and Binary Roles

| Binary | Transport | Primary use |
|--------|-----------|-------------|
| `agent-bus` | CLI direct Redis/PG, `serve --transport stdio`, `serve --transport mcp-http` | Local scripting, MCP stdio for LLM clients |
| `agent-bus-http` | HTTP/REST + SSE (`--port 8400` default), `/mcp` MCP-HTTP endpoint | Long-running service, Windows service install |
| `agent-bus-mcp` | MCP stdio only | MCP-only deployments, lighter footprint |

All three binaries call `bootstrap()` from `agent-bus-core` for PgWriter init,
tracing, and startup announcement.

## Build and Test

```bash
# Workspace sanity check (unit tests, no external deps needed)
cargo test --workspace --lib --bins

# Build all binaries (release)
cargo build --workspace --release --bins

# Clippy (pre-push hook enforces -D warnings)
cargo clippy --workspace --all-targets -- -D warnings

# Format check
cargo fmt --all --check

# Integration tests (requires Redis :6380 + PostgreSQL :5300)
cargo test --workspace --test '*'

# Preferred local build/test entrypoint
pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease
```

Cargo aliases (from `.cargo/config.toml`) still work from the repo root:
`cargo ab-build`, `cargo ab-fast`, `cargo ab-test`, `cargo ab-itest`,
`cargo ab-clippy`, `cargo ab-nextest`. These previously targeted `rust-cli`
and now target `agent-bus-cli` (the `agent-bus` package). Verify with
`cargo ab-build --help` if in doubt.

## Resolved Blockers (vs. Phase 3 Plan)

| Blocker | Resolution |
|---------|------------|
| `McpToolDispatch` needed in core | `crates/agent-bus-core/src/mcp_dispatch.rs` |
| `bootstrap()` needed in core | `crates/agent-bus-core/src/bootstrap.rs` |
| `clap` in core's `Encoding` | Gated behind `features = ["cli"]` |
| `server_mode.rs` was CLI-only | Moved to `crates/agent-bus-cli/src/server_mode.rs` |
| Entry points delegated into `rust-cli` | Each binary crate now owns its `main.rs` |
| Build/deploy scripts anchored on `rust-cli` | Scripts deploy `agent-bus`, `agent-bus-http`, `agent-bus-mcp` directly |

## Remaining / Known Gaps

- **agent-bus-cli local shims**: `lib.rs` still declares private parallel modules
  instead of using `agent-bus-core` types directly. A follow-on cleanup pass
  will remove these shim files once the public API surface of `agent-bus-core`
  is stabilised.
- **Integration test count**: 47 HTTP tests, 6 channel tests, 5 CLI tests.
  Coverage for `/dashboard`, `/tasks/:agent`, and mixed multi-repo scoping
  is still light.
- **Product/protocol gaps** (unchanged from prior status): `thread_id` as
  first-class abstraction, validated task cards, ack deadlines/escalation,
  repo-scoped inbox cursors.
- **Build/deploy scripts** (`scripts/build-deploy.ps1`, `scripts/validate-agent-bus.ps1`):
  may still contain references to `rust-cli` paths internally. Audit and update
  if you encounter `Find-AgentBusBuiltBinary` pointing to the old target tree.

## Documentation Inventory (post-split)

| File | Purpose |
|------|---------|
| `README.md` | Operator overview, crate map, binary roles, build commands |
| `CLAUDE.md` | Architecture reference for agents: four-crate layout, module table |
| `AGENTS.md` | Coding style, testing, commit conventions |
| `AGENT_COMMUNICATIONS.md` | Multi-agent protocol, MCP tool reference |
| `MCP_CONFIGURATION.md` | MCP client setup for Claude/Codex/Gemini |
| `TODO.md` | Active backlog (P0-P6) |
| `agents.TODO.md` | Structural refactor tracking (split complete; remaining cleanup) |
| `docs/phase3-crate-split-plan-2026-04-04.md` | Original split plan (marked COMPLETE) |
| `docs/current-status-2026-04-03.md` | Pre-split snapshot (superseded by this file) |
| `docs/qmd-operator-guide.md` | Operator guide for `qmd` search against repo docs |
