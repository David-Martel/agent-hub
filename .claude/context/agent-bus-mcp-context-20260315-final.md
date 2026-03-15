# Agent Bus MCP Context — 2026-03-15 (Final)

## Project
- **Name:** agent-bus-mcp (Agent Hub)
- **Root:** `~/.codex/tools/agent-bus-mcp`
- **Type:** Rust (MCP server + CLI + HTTP REST)
- **Branch:** main @ 16eb85c
- **Version:** v0.3.0
- **LOC:** 4361 across 12 modules
- **Tests:** 62 (58 unit + 4 integration)

## Current State

Production-grade agent coordination bus deployed across Claude Code, Codex, and Gemini. Dual-backend: Redis (AOF, 100K stream capacity, 512MB) + PostgreSQL (port 5300, trust auth, GIN-indexed tags). Config-driven via `~/.config/agent-bus/config.json`. Features: MCP stdio, HTTP REST with SSE streaming, CLI with 12 subcommands, message schema validation, per-repo journal export, PG circuit breaker with 60s cooldown.

## Session Accomplishments (2026-03-15)

### Agent-Bus Code (15+ commits)
- P3: Module split (2688 LOC monolith → 12 focused modules)
- P1: Settings validation (localhost enforcement, startup checks, health telemetry, PG retry/circuit breaker, log rotation)
- P2: RedisPool for HTTP, spawn_blocking on all handlers
- P4: SSE streaming, prune/export/presence-history subcommands
- P5: README service docs, MCP schema isolation
- Config.json 3-tier loading (file → env → defaults)
- Journal subcommand with PG tag-filtered queries + GIN index
- Message schema validation (finding/status/benchmark)
- Enriched ack responses visible on stdio
- Stream MAXLEN 5000 → 100000

### Infrastructure
- PostgreSQL: redis_backend DB, trust auth, port 5300, performance tuned
- Redis: AOF enabled, 512MB memory, 100K stream
- All 3 MCP configs updated with DATABASE_URL

### stm32-merge (8 commits)
- FPU fpv5-d16 → fpv5-sp-d16 + CMake assertion
- MSC inquiry memcpy bounded with zero-fill
- USB VID 0xCAFE → 0x1209 (pid.codes) + chip UID serial
- LTO enabled, heap 1KB → 4KB, sample rate doc
- lsp_sync.cmake for compile_commands.json
- DHF validation wired into lefthook + CI (blocking)
- 8 safety-critical modules tagged with @requirement
- tinntester-index CI workflow

### Multi-Agent Orchestration
- 4 agent waves, 12+ specialist agents
- 222+ Redis messages, 74+ PG-persisted
- Structured bus reporting (2000 char max, severity tags)
- Cross-repo isolation via repo tags

## Decisions
| Decision | Rationale |
|----------|-----------|
| Config.json over hardcoded defaults | User feedback: URLs shouldn't be hardcoded |
| PG circuit breaker (60s) | Avoid 6s retry overhead per message when PG down |
| NDJSON for journal (not SQLite) | Zero deps, grep-able, git-friendly |
| Trust auth for PG | Local-only, LAN-only bus — password adds friction |
| 100K stream MAXLEN | Redis has 512MB + AOF; 5K was unnecessarily conservative |
| Schema validation optional | Don't break existing agents; opt-in via --schema |

## Next Session Priorities
1. Install agent-bus as proper Windows service (nssm HTTP mode) for minimal startup latency
2. High-performance PG write-through (multi-thread, Arc<Mutex>, zero-copy)
3. Copy AGENT_COMMUNICATIONS.md to stm32-merge, reference in CLAUDE.md
4. Reference AGENT_COMMUNICATIONS.md in ~/CLAUDE.md for all repos
5. Continue stm32-merge @requirement tag coverage (10% → 80%+)
6. Validate bi-directional bus communication patterns
7. Test --server client mode for LAN access
