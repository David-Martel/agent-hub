# Current Status (2026-04-03)

This document is the code-grounded status snapshot for the repository as of
2026-04-03. It is based on the current manifests, Rust source, tests, and
build/deploy scripts rather than on older roadmap text.

## Executive Summary

- The project is functional today: CLI, HTTP, MCP stdio, and MCP Streamable
  HTTP are implemented, along with presence, replayable history,
  direct/group/escalation channels, resource claims, task queues, compact
  context, and dashboard support.
- The repository is in the middle of an architectural migration, not at the
  end of one.
- A Cargo workspace exists and `agent-bus-core` has already absorbed a large
  share of the shared logic.
- The split is still incomplete because `rust-cli` remains the primary runtime
  crate and the `agent-bus-cli`, `agent-bus-http`, and `agent-bus-mcp` crates
  are wrapper crates that depend on `../../rust-cli`.
- The build, validate, and deploy scripts still anchor on `rust-cli` as the
  authoritative crate root.

## Confirmed From Source

### Workspace and crate layout

- Root workspace members are:
  - `rust-cli`
  - `crates/agent-bus-core`
  - `crates/agent-bus-cli`
  - `crates/agent-bus-http`
  - `crates/agent-bus-mcp`
- `agent-bus-core` owns the extracted shared modules:
  - `channels`
  - `codex_bridge`
  - `journal`
  - `models`
  - `ops`
  - `output`
  - `postgres_store`
  - `redis_bus`
  - `settings`
  - `token`
  - `validation`
- `rust-cli` still owns the main runtime and transport surfaces:
  - `lib.rs`
  - `cli.rs`
  - `commands.rs`
  - `http.rs`
  - `mcp.rs`
  - `server_mode.rs`
  - `monitor.rs`
  - integration tests under `rust-cli/tests/`
- Several legacy `rust-cli` modules are now compatibility shims into
  `agent-bus-core`:
  - `rust-cli/src/channels.rs`
  - `rust-cli/src/redis_bus.rs`
  - `rust-cli/src/postgres_store.rs`
  - `rust-cli/src/ops/mod.rs`
- The surface crates are not yet independent implementations:
  - `crates/agent-bus-cli/src/main.rs` calls `agent_bus::main_entry()`
  - `crates/agent-bus-http/src/main.rs` calls `agent_bus::http_entry()`
  - `crates/agent-bus-mcp/src/main.rs` calls `agent_bus::mcp_entry()`
  - all three depend on `agent-bus = { path = "../../rust-cli", ... }`

### Transport and product surface

- CLI supports health, send/read/watch/ack, presence, export/journal, service
  controls, direct/group channel commands, claim lifecycle, compact context,
  dedup, session summary, and task queue operations.
- HTTP exposes:
  - `/health`
  - `/messages`, `/messages/batch`, `/messages/{id}/ack`
  - `/presence`, `/presence/{agent}`, `/presence/history`
  - `/events`, `/events/{agent_id}`, `/notifications/{agent_id}`
  - `/pending-acks`
  - `/channels/direct/*`, `/channels/groups/*`, `/channels/escalate`,
    `/channels/arbitrate/*`, `/channels/summary`
  - `/token-count`, `/compact-context`, `/tasks/{agent}`, `/dashboard`
  - `/admin/service`, `/admin/service/control`
  - `/mcp`
- MCP exposes the tool surface implemented in `rust-cli/src/mcp.rs`, including
  health, message reads/writes, presence, channel operations, claims, and
  `check_inbox`.

### Query and coordination model

- `repo`, `session`, `tag`, and `thread_id` are implemented as filter scopes.
- `thread_id` is currently metadata plus filtering/indexing, not a first-class
  thread membership or summary abstraction.
- Task queue payloads are still opaque strings rather than validated task cards.
- Pending acknowledgements exist with TTL and stale detection, but not with
  first-class deadlines/reminders/escalation workflows.

### Build and operator reality

- `build.ps1`, `scripts/build-deploy.ps1`, and
  `scripts/validate-agent-bus.ps1` still set `rust-cli` as the main build root.
- Those scripts locate binaries via `Find-AgentBusBuiltBinary` under the
  `rust-cli` target tree.
- That means the workspace split is real but operationally incomplete.

## Validation Snapshot

- `cargo test --workspace --lib --bins` passed on 2026-04-03 during this
  review.
- Unit coverage is strong in both `agent-bus` and `agent-bus-core`.
- Integration suites exist for Redis, HTTP, and channels, but they are
  environment-dependent and skip when Redis or the HTTP service is not running.

## Required Remaining Work

### Structural

- Finish moving transport-agnostic behavior behind `agent-bus-core`.
- Reduce the remaining transport-local orchestration in:
  - `rust-cli/src/commands.rs`
  - `rust-cli/src/http.rs`
  - `rust-cli/src/mcp.rs`
- Finish the crate split so `agent-bus-cli`, `agent-bus-http`, and
  `agent-bus-mcp` stop depending on `rust-cli`.
- Re-anchor build/deploy/bootstrap scripts so they target the split workspace
  directly rather than treating `rust-cli` as the only authoritative crate.

### Product / protocol

- Add explicit subscription records and resource/topic scope subscriptions.
- Promote `thread_id` into a joinable conversation/thread abstraction with
  dedicated summaries and compaction.
- Replace raw task queue strings with validated task cards.
- Add ack deadlines, reminders, and escalation semantics beyond the current
  pending-ack TTL.
- Add repo/session inventory surfaces and repo-scoped inbox cursors.

### Validation

- Add CLI server-mode tests for `AGENT_BUS_SERVER_URL`.
- Add end-to-end MCP tool tests.
- Add mixed multi-repo scoping tests.
- Add PostgreSQL `jsonb` regression coverage for read/compaction paths.
- Add CLI/HTTP parity coverage around direct-channel and compact-context flows.
- Add missing HTTP integration coverage for `/dashboard`, `/tasks/:agent`, and
  `/token-count`.

## Documentation Inventory

These files should be read as follows after this update:

- `README.md`
  - Operator-facing overview and build/runtime guidance.
- `TODO.md`
  - Active backlog status, including open feature and structural work.
- `agents.TODO.md`
  - Canonical completion plan for the remaining structural refactor.
- `AGENTS.md`
  - Short repo guidance for coding agents.
- `CLAUDE.md`
  - Claude-specific system guidance and repo architecture notes.
- `MCP_CONFIGURATION.md`
  - How to connect MCP clients and what transport surfaces actually exist.
- `AGENT_COMMUNICATIONS.md`
  - Coordination protocol and prompt-facing agent guidance.
- `IMPLEMENTATION_NOTES.md`
  - Interop/serialization notes plus architecture checkpoint.
- `docs/structural-refactor-plan-2026-03-25.md`
  - Historical refactor plan with an added current-status checkpoint.
- `docs/public-release-checklist.md`
  - Publish gate; should be read together with this status note.

Historical planning/assessment docs remain useful for background but should not
be treated as the current source of truth without checking this file first.
