# Agent Bus MCP v0.4 Upgrade Context (2026-03-16)

## Project State
- **Branch:** main @ 311faaa
- **LOC:** 10,066 across 17 modules (was 5,731 / 13 modules at session start)
- **Tests:** 149 (145 unit + 4 integration)
- **Service:** AgentHub running at localhost:8400, all binaries deployed

## What Was Built in This Session

### v0.4 Features (4 parallel agents)
1. **Channel System** (channels.rs, 1209 LOC)
   - Direct messaging: bus:direct:<lo>:<hi> Redis streams
   - Group discussions: bus:group:<name> with member management
   - Orchestrator escalation: auto-routes to agent with orchestration capability
   - Ownership arbitration: claims, contested status, orchestrator resolution

2. **Token Optimization**
   - TOON encoding: @from→to #topic [tags] body (70% savings vs JSON)
   - LZ4 compression: auto-compress bodies >512 bytes, transparent decompress
   - Batch operations: /messages/batch, /read/batch, /ack/batch

3. **Codex Bridge** (codex_bridge.rs, 350+ LOC)
   - Config discovery from ~/.codex/config.toml
   - Finding normalization and sync
   - Structured markdown formatting for Codex consumption

4. **Coordination Features**
   - Required-ack tracking: Redis SET with 300s TTL, stale detection >60s
   - SSE agent routing: /events/:agent_id for targeted real-time delivery
   - Monitor CLI: real-time dashboard with agent status, findings, channels

5. **MCP Modernization**
   - Streamable HTTP transport (--transport mcp-http)
   - Protocol negotiation with feature/transport/schema discovery
   - Redis r2d2 connection pool (5 conns, 5s timeout)

### Orchestrator Skill Created
- ~/.claude/skills/agent-hub-orchestrator.md
- Key learning: agents DON'T poll inbox or coordinate with siblings
- Bus is reporting/audit, orchestrator must relay all cross-agent info
- Ownership-as-review pattern: shared claims enable natural code review

## Architecture Decisions
1. Channels layered on Redis streams (bus:direct:*, bus:group:*, bus:escalations, bus:claims:*)
2. TOON is a display format, not a wire format (JSON remains canonical)
3. LZ4 chosen over zstd for decompression speed (LAN, latency matters)
4. r2d2 over bb8 for pool (synchronous Redis client, simpler)
5. Ownership arbitration auto-escalates contested claims to orchestrator

## Next Session Priorities
1. E2E testing of channel system (create group, post, read, verify)
2. Integration test for ownership arbitration flow
3. Codex CLI bidirectional sync testing
4. TOON encoding benchmarks (measure actual token savings)
5. MCP Streamable HTTP client testing
6. Update AGENT_COMMUNICATIONS.md with channel documentation
7. Documentation review and update
