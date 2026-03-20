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
- [x] MessagePack encoding added (`encode_msgpack`/`decode_msgpack` in output.rs).
- [x] LZ4 compression for bodies >512 bytes (auto-applied, transparent decompression).
- [ ] Package the native binary and scripts for reproducible machine bootstrap.
- [ ] Add optional WinSW support if NSSM behavior proves insufficient.
- [ ] Add A2A adapter mapping once the core schema settles.

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
- [x] **Broadcast inclusion bug**: Investigated — HTTP broadcast param defaults correctly. Issue was timing (message arrived after orchestrator's last read). Documented workaround: send to specific agent instead of `all`.
- [x] **Auto-schema inference from topic**: `infer_schema_from_topic()` in validation.rs, wired into CLI, MCP, and HTTP paths. `*-findings` → `finding`, `status`/`ownership`/`coordination` → `status`, `benchmark` → `benchmark`.
- [x] **Schema auto-fitter**: `auto_fit_schema()` wraps plain bodies with FINDING:/SEVERITY: headers, detects severity from keywords (CRITICAL/HIGH/MEDIUM/LOW). 12 new tests.
- [x] **Auto-deploy AGENT_COMMUNICATIONS.md**: `journal.rs` deploys protocol doc to repo root on first journal export if missing.
- [x] **Ownership conflict detection**: When an agent posts `topic=ownership`, the bus should track claimed files. Subsequent edits to claimed files by OTHER agents should generate a warning.

**Performance and reliability:**
- [x] Async PG write-through via tokio mpsc channel (fire-and-forget, batched)
- [x] Proactive circuit breaker reset (periodic health check, not just on explicit `health` call)
- [ ] Agent task queue (orchestrator queues tasks; agents pull when idle — enables wave dispatch without manual timing)

**Features:**
- [x] Finding deduplication command (group journal entries by file path, merge overlapping)
- [x] Session ID auto-generation (env var `AGENT_BUS_SESSION_ID` set by orchestrator)
- [x] Session summary auto-generation (from all bus messages with matching session tag)
- [ ] `--server` client mode (CLI → HTTP → Redis) for LAN access from other machines
- [x] Message threading enforcement (auto-link finding→fix→verify chains)
- [x] TOON/MessagePack exploration for token-efficient message encoding
- [ ] Agent inbox notification — MCP server should push notifications when new messages arrive

### P7 Documentation and protocol (observed from finance-warehouse live session)

- [x] MCP tool names section in AGENT_COMMUNICATIONS.md
- [x] Orchestration patterns (parallel analysis, chained tasks, cross-repo, session recovery)
- [x] Ownership claims as mandatory protocol step (emergent from finance-warehouse agents)
- [x] Polling strengthened as numbered step in Quick Start
- [x] Per-repo AGENT_COMMUNICATIONS.md auto-deploy (journal.rs deploys on first export)
- [x] MCP tool descriptions include protocol instructions in server `instructions` field
- [x] AGENT_COORDINATION.md updated for v0.4+ protocol in ~/.claude/ and ~/.codex/
- [x] CLI --help expanded with TOON encoding, AGENT_BUS_SESSION_ID, new command examples
- [ ] Agent prompt template library (pre-built prompts for common agent types with bus instructions)
- [ ] Orchestrator monitoring dashboard (read bus, show agent status, findings by severity)

### Completed in 2026-03-19 sprint (5 parallel workstreams)

- [x] Port PyO3 token estimation to Rust (`token.rs`: `estimate_tokens()`)
- [x] Port PyO3 context compaction to Rust (`token.rs`: `compact_context()`)
- [x] Port PyO3 message minimization to Rust (`token.rs`: `minimize_message()`)
- [x] Add MessagePack encoding/decoding (`output.rs`: `encode_msgpack()`/`decode_msgpack()`)
- [x] Add `token-count` CLI subcommand + `POST /token-count` HTTP endpoint
- [x] Add `compact-context` CLI subcommand + `POST /compact-context` HTTP endpoint
- [x] Python agent-bus-mcp fully deprecated — single Rust binary
- [x] Proactive PG circuit breaker reset (background health monitor, 30s interval)
- [x] PG write-through metrics (`PgWriteMetrics`: queued/written/batches/errors)
- [x] PG write metrics exposed in health endpoint
- [x] Ownership conflict detection (`OwnershipTracker` in channels.rs)
- [x] File path extraction from ownership claim bodies (regex-lite)
- [x] Mandatory schema enforcement on MCP and HTTP transports
- [x] Session ID auto-generation via `AGENT_BUS_SESSION_ID` env var
- [x] Auto-tag messages with `session:<id>` when session ID is set
- [x] `session-summary` CLI subcommand (agent count, topic distribution, severity)
- [x] `dedup` CLI subcommand (finding deduplication by file path)
- [x] Message threading: auto-infer `thread_id` from `reply_to`
- [x] E2E channel integration tests (direct, group, claim/resolve)
- [x] Criterion benchmark harness (token estimation, TOON, MessagePack)

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

### Completed in 2026-03-20 session (post-sprint hardening)

- [x] Deploy v0.3+ binary with all sprint features to ~/bin/agent-bus.exe
- [x] Restart AgentHub service with new binary, verify health (PG metrics visible)
- [x] Validate all new HTTP endpoints: /token-count, /compact-context
- [x] Validate new CLI commands: token-count, compact-context, session-summary, dedup
- [x] Test claim/resolve lifecycle: granted → contested → resolved
- [x] Test direct channel message round-trip
- [x] Test session ID auto-tagging via AGENT_BUS_SESSION_ID env var
- [x] Test all 5 encoding modes (json, compact, minimal, toon, human)
- [x] Update Claude mcp.json: description, version 2.11.0, changelog
- [x] Update AGENT_COORDINATION.md in ~/.claude/ and ~/.codex/ (v0.4+ protocol)
- [x] Update AGENT_BUS.md in ~/.codex/docs/
- [x] Update DEPRECATION.md: Python fully deprecated
- [x] Add AGENT_BUS_SESSION_ID + AGENT_BUS_CONFIG to CLI --help env vars
- [x] Add TOON to CLI --help ENCODING MODES
- [x] Expand CLI --help EXAMPLES with new commands

- [x] Replace ~/.codex/tools/agent-bus-mcp/ with junction to C:\codedev\agent-bus
- [x] Redirect service logs from old dir to C:\ProgramData\AgentHub\logs\
- [x] Deploy AGENT_COORDINATION.md to stm32-merge, finance-warehouse, Gen3_0
- [x] Broadcast protocol update notifications to all agents via bus
- [x] Add AGENT_BUS_SESSION_ID to CLAUDE.md env vars table

### Completed in 2026-03-20 testing session

- [x] Fix SSH `pageant.conf` PowerShell prompt injection (Match exec -NoProfile)
- [x] Redis-backed ownership conflict detection at `send` layer (cross-process)
- [x] GitHub Actions CI/CD workflow (self-hosted runner: fmt, clippy, test, build, audit, bench)
- [x] PG retry/circuit breaker unit tests (25 tests: state machine, retry exhaustion, metrics)
- [x] Redis decode/compression unit tests (30 tests: LZ4, stream decode, prepare_message, schema)
- [x] Models serialization tests (12 tests: round-trip, SmallVec tags, Health optional fields)
- [x] CLI argument parsing tests (23 tests: all subcommands, positional args, defaults)
- [x] MCP tool list tests (9 tests: tool count, required fields, schema validation)
- [x] Test count: 282 → 384 (340 unit + 44 integration)

### P8 Future development plans (from sprint observations)

**Architecture:**
- [ ] Agent task queue — push-based dispatch from orchestrator to agents (agents don't self-poll)
- [ ] `--server` client mode (CLI → HTTP → Redis) for LAN/multi-machine access
- [ ] Agent inbox notification — MCP push when new messages arrive for an agent

**Performance:**
- [ ] Redis-only vs Redis+PG throughput benchmarks (criterion, end-to-end)
- [ ] PG query plan review with real message volumes (3500+ messages now available)
- [ ] Connection pool tuning based on actual load patterns

**Documentation:**
- [ ] Agent prompt template library (pre-built .md prompts with bus integration)
- [ ] Orchestrator monitoring web dashboard (read bus, show agent status grid)

**Packaging:**
- [ ] Bootstrap script: install binary, configure Redis/PG, create service, validate
- [ ] GitHub Release with prebuilt binaries (Windows x64)
- [ ] `cargo install agent-bus` via crates.io
- [ ] Self-hosted GitHub Actions runner setup documentation

## Suggested execution order

1. ~~Finish Rust-side localhost validation and connection pooling.~~ Done.
2. ~~Split `main.rs` into modules without changing behavior.~~ Done.
3. ~~Add integration tests that exercise Redis and PostgreSQL together.~~ Done.
4. ~~Add HTTP streaming and remote client mode.~~ SSE streaming done. `--server` client mode deferred.
5. ~~Multi-repo validation (stm32-merge, finance-warehouse).~~ Done. 320+ PG messages.
6. ~~Async PG write-through + finding dedup + default schema enforcement.~~ Done (2026-03-19 sprint).
7. ~~Token estimation, context compaction, MessagePack, TOON.~~ Done (2026-03-19 sprint WS1).
8. ~~E2E channel tests + Criterion benchmark harness.~~ Done (2026-03-19 sprint WS5).
9. Agent task queue (orchestrator → agent wave dispatch without manual timing).
10. `--server` client mode (CLI → HTTP → Redis) for LAN access from other machines.
11. Agent inbox notification (MCP push when new messages arrive).
12. Reassess wire-format and packaging work after the runtime stabilizes.
