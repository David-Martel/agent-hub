# Agent Hub — Detailed TODO

## Current state

### Completed foundation
- [x] Retire the Python CLI path and keep Rust as the active runtime.
- [x] Move local wrappers to `pwsh` and the Rust binary.
- [x] Add HTTP transport alongside MCP stdio.
- [x] Default all local endpoints to `localhost`.
- [x] Integrate PostgreSQL durable storage into the Rust runtime.
- [x] Mirror message writes into `agent_bus.messages`.
- [x] Mirror presence writes into `agent_bus.presence_events`.
- [x] Prefer PostgreSQL for history reads and fall back to Redis on failure.
- [x] Redact credentials from health output.
- [x] Add local setup, validation, and Windows service install/remove scripts.
- [x] Add native Windows service install via `nssm`.

### Local machine deployment
- [x] Redis service: `Redis` on `redis://localhost:6380/0`
- [x] PostgreSQL service: `postgresql-x64-18` on `localhost:5432`
- [x] Native binary install path: `C:\Users\david\bin\agent-bus.exe`
- [x] Native Windows service path: `scripts/install-agent-hub-service.ps1`

## Next priorities

### P1 Runtime hardening
- [x] Enforce `localhost` for `AGENT_BUS_REDIS_URL`, `AGENT_BUS_DATABASE_URL`, and `AGENT_BUS_SERVER_HOST` inside the Rust settings path, not just in wrapper defaults.
- [x] Add explicit startup validation for invalid stream/table names and fail early with actionable errors.
- [x] Add health fields for Redis stream length and PostgreSQL row counts when `--encoding json` is requested.
- [x] Add bounded retry/backoff for PostgreSQL persistence failures instead of reconnecting on every write.
- [x] Add service log rotation or size limits for `C:\ProgramData\AgentHub\logs\agent-hub-service.log`.

### P2 Performance
- [x] Replace one-connection-per-call PostgreSQL access with retry-based reconnection (thread-local pool deferred until benchmarks justify).
- [x] Move blocking Redis/PostgreSQL work behind `spawn_blocking` or dedicated worker threads in HTTP handlers.
- [x] Reuse Redis clients/connections in HTTP mode instead of reconnecting per request.
- [ ] Add benchmarks for `send`, `read`, `watch`, and HTTP `/messages` throughput with Redis-only versus Redis+Postgres modes.
- [ ] Add index review based on actual query plans once message volume is non-trivial.

### P3 Architecture
- [x] Split `rust-cli/src/main.rs` into modules:
  - `settings.rs`
  - `models.rs`
  - `redis_bus.rs`
  - `postgres_store.rs`
  - `output.rs`
  - `cli.rs`
  - `commands.rs`
  - `validation.rs`
  - `mcp.rs`
  - `http.rs`
- [x] Move shared request validation and JSON argument parsing into reusable helpers.
- [x] Isolate MCP tool schemas from tool execution logic.
- [x] Add an integration-test harness for live Redis/PostgreSQL behavior under `rust-cli/tests/`.

### P4 Features
- [x] SSE or websocket live streaming endpoint for HTTP clients.
- [ ] `--server` client mode for remote/local HTTP calls instead of direct Redis access.
- [x] Presence read path backed by PostgreSQL history for forensic/debug use.
- [x] Optional retention management task for PostgreSQL history pruning.
- [x] Optional message replay or export command for recovery workflows.

### P5 Interop and packaging
- [x] Add Windows service health/restart troubleshooting docs to `README.md`.
- [ ] Package the native binary and scripts for reproducible machine bootstrap.
- [ ] Add optional WinSW support if NSSM behavior proves insufficient.
- [ ] Add A2A adapter mapping once the core schema settles.
- [ ] Revisit MessagePack/LZ4 only after runtime and storage shape stop moving.

### Completed in 2026-03-15 session

- [x] Config.json 3-tier loading (file → env → defaults)
- [x] PG circuit breaker (60s cooldown after confirmed outage)
- [x] PG shared client pool (get/return pattern, eliminates TCP-per-call)
- [x] Journal subcommand (per-repo NDJSON export with PG tag queries + GIN index)
- [x] Message schema validation (finding/status/benchmark) on CLI, MCP, HTTP
- [x] Enriched ack responses (ack_sent, timestamp visible on stdio)
- [x] Sync subcommand (Redis→PG backfill for historical messages)
- [x] SSE streaming endpoint (GET /events)
- [x] Windows service install via nssm (direct Rust binary, auto-restart)
- [x] Build-deploy.ps1 script (build→copy→restart→healthcheck)
- [x] Lefthook + ast-grep enforcement (pre-commit, pre-push)
- [x] Stream MAXLEN 5000 → 100000
- [x] Redis AOF + PG trust auth infrastructure
- [x] AGENT_COMMUNICATIONS.md with MCP tool names, orchestration patterns, polling protocol
- [x] Spawn_blocking on all HTTP handlers

### P6 Learnings from multi-repo deployment (finance-warehouse, stm32-merge)

**Bus behavior fixes (from live observation):**
- [ ] **Broadcast inclusion bug**: Messages `to: all` don't appear in `list_messages(agent=claude)` via HTTP GET — the `include_broadcast` parameter defaults to true on CLI but the HTTP `broadcast` query param may not be checked. Finance-warehouse orchestrator couldn't see hub-admin messages sent to `all`. VERIFY AND FIX.
- [ ] **Auto-schema inference from topic**: Agents forget to set schema. Infer from topic name: `*-findings` → `finding`, `status`/`ownership`/`coordination` → `status`, `benchmark` → `benchmark`. Apply in `bus_post_message`.
- [ ] **Ownership conflict detection**: When an agent posts `topic=ownership`, the bus should track claimed files. Subsequent edits to claimed files by OTHER agents should generate a warning.

**Performance and reliability:**
- [ ] Async PG write-through via tokio mpsc channel (fire-and-forget, batched)
- [ ] Proactive circuit breaker reset (periodic health check, not just on explicit `health` call)
- [ ] Agent task queue (orchestrator queues tasks; agents pull when idle — enables wave dispatch without manual timing)

**Features:**
- [ ] Finding deduplication command (group journal entries by file path, merge overlapping)
- [ ] Session ID auto-generation (env var `AGENT_BUS_SESSION_ID` set by orchestrator)
- [ ] Session summary auto-generation (from all bus messages with matching session tag)
- [ ] `--server` client mode (CLI → HTTP → Redis) for LAN access from other machines
- [ ] Message threading enforcement (auto-link finding→fix→verify chains)
- [ ] TOON/MessagePack exploration for token-efficient message encoding
- [ ] Agent inbox notification — MCP server should push notifications when new messages arrive

### P7 Documentation and protocol (observed from finance-warehouse live session)

- [x] MCP tool names section in AGENT_COMMUNICATIONS.md
- [x] Orchestration patterns (parallel analysis, chained tasks, cross-repo, session recovery)
- [x] Ownership claims as mandatory protocol step (emergent from finance-warehouse agents)
- [x] Polling strengthened as numbered step in Quick Start
- [ ] Per-repo AGENT_COMMUNICATIONS.md auto-deploy (create on first `journal` export)
- [ ] MCP tool description improvements (include schema examples in tool descriptions)
- [ ] Agent prompt template library (pre-built prompts for common agent types with bus instructions)
- [ ] Orchestrator monitoring dashboard (read bus, show agent status, findings by severity)

### Observations from finance-warehouse live session (2026-03-15)

**What works:**
- Ownership claims: agents claim files before editing, check for conflicts, linter skips owned files
- Wave-based dispatch: orchestrator dispatches wave 1, waits for completion, dispatches wave 2
- Presence tracking: 8 agents registered with capabilities across 2 repos
- Cross-agent awareness: rust-builder notes tax-integrator's ownership, avoids conflict
- Completion signals: all agents post structured COMPLETE summaries

**What doesn't work:**
- Schema adoption: 0/18 finance-warehouse messages used schema field
- Inbox polling: 0 agents checked for follow-up tasks during execution
- Broadcast visibility: messages to `all` not visible when querying by specific agent
- Hub-admin announcement not seen by finance-warehouse orchestrator (polling + broadcast bug)

**Message rate:** 0.7 msgs/min across 6 agents. Acceptable but could batch findings.
**Session duration:** 27 min for 18 messages (6 agents, 4 ownership + 9 status + 5 completions)

## Suggested execution order

1. ~~Finish Rust-side localhost validation and connection pooling.~~ Done.
2. ~~Split `main.rs` into modules without changing behavior.~~ Done.
3. ~~Add integration tests that exercise Redis and PostgreSQL together.~~ Done.
4. ~~Add HTTP streaming and remote client mode.~~ SSE streaming done. `--server` client mode deferred.
5. ~~Multi-repo validation (stm32-merge, finance-warehouse).~~ Done. 320+ PG messages.
6. Async PG write-through + finding dedup + default schema enforcement.
7. Reassess wire-format and packaging work after the runtime stabilizes.
