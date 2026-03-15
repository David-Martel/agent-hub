# Agent Bus MCP Context — 2026-03-15 Session 2

## Project State
- **Branch:** main @ 7213f05
- **LOC:** ~4500 across 13 modules (journal.rs added)
- **Tests:** 64 (60 unit + 4 integration)
- **Service:** AgentHub running at localhost:8400 (nssm, auto-start)
- **Bus:** Redis 296 stream / PG 304 msgs, fully synced

## This Session

### Infrastructure
- PostgreSQL: redis_backend DB, trust auth, port 5300, GIN index on tags
- Redis: AOF enabled, 512MB, 100K MAXLEN
- Service: nssm direct binary (no PS wrapper), auto-restart on failure
- Config: 3-tier loading (config.json → env → defaults)
- All 3 MCP configs (Claude/Codex/Gemini) updated with DATABASE_URL

### Agent-Bus Features Added
- journal.rs: per-repo NDJSON export with idempotent cursor + PG tag queries
- validation.rs: message schema validation (finding/status/benchmark)
- postgres_store.rs: shared client pool (get/return), circuit breaker, sync backfill
- mcp.rs: enriched ack responses, schema param on post_message
- cli.rs: 13 subcommands (added journal, sync, prune, export, presence-history)
- http.rs: spawn_blocking on all handlers, SSE streaming, schema validation
- settings.rs: config.json loading, 100K MAXLEN, localhost enforcement
- build-deploy.ps1: automated build→deploy→restart pipeline

### Multi-Agent Orchestration (20+ agents this session)
- 4 agent waves on stm32-merge: analysis, P0 fixes, IEC 62304, enforcement, dedup/UVC
- 304 PG-persisted messages, 116 in stm32-merge journal
- HTTP POST validated as primary agent communication path
- Bi-directional task assignment tested (works, but agents don't poll inbox)

## Agent Registry
| Wave | Agents | Findings | Key Results |
|------|--------|----------|-------------|
| Analysis | architect, security, performance | 18 | FPU mismatch, IEC 62304 gaps, USB VID |
| P0 Fixes | c-pro, security, performance | 7 fixes | FPU, memcpy, VID, serial, LTO, heap |
| IEC 62304 | iec62304-reviewer, build-architect | 26 | 6% req coverage, non-blocking CI, lsp_sync |
| Enforcement | cicd-engineer, req-tagger, test-integrator | 11 fixes | DHF blocking, 8 modules tagged, CI workflow |
| Dedup/UVC | dedup-auditor, rust-reviewer, uvc-analyst, quality-reviewer | 50 | 17 duplications, VID/PID P0, zero C++ tests |

## Next Priorities
1. Fix VID/PID mismatch (29 host refs still use 0xCAFE)
2. Delete 8 safe-to-remove duplicate Python files
3. Unify Rust workspaces (tools/rust/ → stm32-remote/)
4. Remove || true from ALL CI test workflows
5. Create C++ GoogleTest unit tests for audio_engine, codec_wm8994
6. High-perf PG: async write-through with tokio channels
7. Agent polling protocol: teach agents to check inbox during work
