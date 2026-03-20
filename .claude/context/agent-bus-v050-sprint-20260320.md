# Agent Bus v0.5.0 Sprint Context (2026-03-20)

## Project State
- **Branch:** main @ 1c7bbcb
- **Version:** 0.5.0
- **LOC:** ~15,500 across 18 modules
- **Tests:** 419 (375 unit + 44 integration)
- **Service:** AgentHub running at localhost:8400
- **Bus:** Redis 3912 / PG 3904 messages, 899 presence records

## What Was Built in This Session

### Sprint (5 parallel workstreams, 12 commits)
1. **WS1 — PyO3 Port:** token.rs (estimate_tokens, minimize_message, compact_context), MessagePack encode/decode, token-count + compact-context CLI + HTTP endpoints
2. **WS2 — Async PG:** Proactive circuit breaker reset (30s background health monitor), PgWriteMetrics (queued/written/batches/errors), metrics in health output
3. **WS3 — Ownership:** OwnershipTracker (file-level conflict detection), extract_claimed_files (regex-lite), mandatory schema enforcement on MCP/HTTP
4. **WS4 — Sessions:** AGENT_BUS_SESSION_ID auto-tagging, session-summary command, dedup command, thread_id auto-inference from reply_to
5. **WS5 — Testing:** E2E channel integration tests, criterion benchmark harness, TODO.md update

### Post-Sprint Features (8 commits)
- Agent task queue: push/pull/peek via CLI + HTTP (/tasks/:agent)
- --server client mode: CLI -> HTTP via AGENT_BUS_SERVER_URL (reqwest, feature-gated)
- check_inbox MCP tool: cursor-based inbox polling (14th tool)
- E2E Redis I/O benchmarks (criterion)
- PG timestamp indexes (eliminated seq-scan on health path)
- Monitoring web dashboard (GET /dashboard)
- Bootstrap install script (scripts/bootstrap.ps1)
- GitHub Release workflow + crates.io metadata

### Infrastructure
- ~/.codex/tools/agent-bus-mcp/ replaced with junction to C:\codedev\agent-bus
- Service logs redirected to C:\ProgramData\AgentHub\logs\
- SSH pageant.conf fixed (-NoProfile on Match exec)
- Python fully deprecated — single Rust binary
- CI/CD: GitHub Actions (self-hosted, 7 jobs)
- AGENT_COORDINATION.md deployed to stm32-merge, finance-warehouse, Gen3_0

### Test Expansion (282 -> 419)
- PG retry/circuit breaker: 25 tests
- Redis decode/compression: 30 tests
- Models serialization: 12 tests
- CLI argument parsing: 23 tests
- MCP tool list: 9+ tests
- Dashboard HTML: 6 tests

## Architecture Decisions
1. Task queue uses Redis LIST (RPUSH/LPOP) — simpler than streams for FIFO work dispatch
2. Server mode uses reqwest behind a cargo feature flag — binary size unaffected when disabled
3. check_inbox uses Redis cursor keys (bus:cursor:<agent>) — stateless across process restarts
4. Dashboard is self-contained HTML (no external deps) — served as a Rust string
5. Ownership conflict detection checks both Redis claims AND in-memory tracker
6. Schema enforcement is mandatory on MCP/HTTP, optional on CLI

## CLI Commands (27 total)
health, send, read, watch, ack, presence, presence-list, serve, prune, export, presence-history, journal, sync, monitor, batch-send, pending-acks, claim, claims, resolve, codex-sync, session-summary, dedup, token-count, compact-context, push-task, pull-task, peek-tasks

## MCP Tools (14 total)
bus_health, post_message, list_messages, ack_message, set_presence, list_presence, list_presence_history, negotiate, create_channel, post_to_channel, read_channel, claim_resource, resolve_claim, check_inbox

## HTTP Endpoints (26+)
GET /health, POST /messages, GET /messages, POST /messages/batch, POST /messages/:id/ack, GET /read/batch, POST /ack/batch, PUT /presence/:agent, GET /presence, GET /presence/history, GET /events, GET /events/:agent_id, GET /pending-acks, POST /channels/direct/:agent, GET /channels/direct/:agent, POST /channels/groups, GET /channels/groups, POST /channels/groups/:name/messages, GET /channels/groups/:name/messages, POST /channels/escalate, POST /channels/arbitrate/:resource, GET /channels/arbitrate/:resource, POST /channels/arbitrate/:resource/resolve, POST /token-count, POST /compact-context, POST /tasks/:agent, GET /tasks/:agent, DELETE /tasks/:agent, GET /dashboard

## All TODO Items: COMPLETE
No remaining open items. Future work tracked in GitHub Issues.
