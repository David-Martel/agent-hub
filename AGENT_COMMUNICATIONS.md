# Agent Hub Communication Protocol

> **This document MUST be included in every subagent's system prompt.**
> Agents that do not follow this protocol will have their findings rejected.

## Quick Start (Copy This Into Your First Action)

```bash
# 1. Announce yourself
curl -s -X POST http://localhost:8400/messages -H "Content-Type: application/json" \
  -d '{"sender":"<YOUR-AGENT-ID>","recipient":"claude","topic":"status","body":"Online and starting work on <TASK>","tags":["repo:<REPO-NAME>"]}'

# 2. Post findings (MANDATORY schema — messages without schema are rejected)
curl -s -X POST http://localhost:8400/messages -H "Content-Type: application/json" \
  -d '{"sender":"<YOUR-AGENT-ID>","recipient":"claude","topic":"<type>-findings","body":"FINDING: <summary>\nSEVERITY: HIGH\nFILE: <path:line>\nCURRENT: <what exists>\nPROPOSED: <what should change>\nSTATUS: discovered","tags":["repo:<REPO-NAME>","severity:high"],"schema":"finding"}'

# 3. Check your inbox (DO THIS EVERY 2-3 TOOL CALLS)
curl -s "http://localhost:8400/messages?agent=<YOUR-AGENT-ID>&since=5&limit=5"

# 4. Post completion
curl -s -X POST http://localhost:8400/messages -H "Content-Type: application/json" \
  -d '{"sender":"<YOUR-AGENT-ID>","recipient":"claude","topic":"<type>-findings","body":"COMPLETE: <N> findings. CRITICAL=<n>, HIGH=<n>, MEDIUM=<n>, LOW=<n>. Key: <one-line summary>","tags":["repo:<REPO-NAME>","status:complete"],"schema":"finding"}'
```

## Message Schemas (MANDATORY)

Every message MUST include a `"schema"` field. Three schemas are available:

### Schema: `finding` (for code review, analysis, fixes)

```
FINDING: Buffer overflow in MSC inquiry memcpy
SEVERITY: HIGH
FILE: src/usb/usb_msc_callbacks.cpp:64
CURRENT: memcpy(vendor_id, vid, strlen(vid)) — no bounds check
PROPOSED: memset + TU_MIN bounded copy per SCSI spec
RATIONALE: strlen could exceed 8-byte field, corrupting stack
STATUS: discovered
```

Required fields: `FINDING:` + `SEVERITY:`, OR `FIX` keyword, OR `COMPLETE` keyword.
Severity levels: `CRITICAL`, `HIGH`, `MEDIUM`, `LOW`, `INFO`
Status values: `discovered`, `proposed`, `fixed`, `verified`

### Schema: `status` (for coordination, ownership, handoffs)

```
Working on audio_engine.cpp analysis. Found 3 issues so far. ETA: 2 more minutes.
```

Required: non-empty body. Use for progress updates, ownership claims, handoffs.

### Schema: `benchmark` (for metrics, performance data)

```
agents=4, findings=50, duration=12min, tokens=450K, bus_msgs=55, pg_persisted=55
```

Required: contains `key=value` or `key:value` pairs.

## Polling Protocol (MANDATORY)

### For Subagents

**You MUST check your inbox every 2-3 tool calls:**

```bash
curl -s "http://localhost:8400/messages?agent=<YOUR-AGENT-ID>&since=5&limit=5"
```

If you receive a message with `topic: "task-assignment"` or `topic: "follow-up-task"`:
1. Acknowledge it: post a status message saying you received it
2. Incorporate the instruction into your current work
3. If it conflicts with your current task, post a status explaining why

If you receive a message with `topic: "coordination"`:
1. Read it for context about what other agents are doing
2. Adjust your work to avoid duplicating their findings

### For Orchestrators

When dispatching agents, include this in their prompt:
```
## BUS COMMUNICATION (MANDATORY)
Report via HTTP POST to http://localhost:8400/messages.
Schema is REQUIRED on all messages. Check your inbox every 2-3 tool calls.
Your agent ID is: <AGENT-ID>
```

After dispatching agents:
1. Monitor the bus: `curl -s "http://localhost:8400/messages?agent=claude&since=5"`
2. Send follow-up tasks when findings converge: POST to `recipient:<agent-id>`
3. Send cross-agent coordination when agents should know about each other's work
4. Post session benchmarks when all agents complete

## Accelerating Analysis

### Use Available Tools (Don't Re-scan)

Before reading files, check if these tools have already indexed the repo:

| Tool | What it does | How to use |
|------|-------------|------------|
| `git-cluster-analyzer` | Scans dirty files, proposes commit clusters | MCP tool: `scan_repos`, `propose_clusters` |
| `tinntester-index` | Indexes source files, extracts REQ tags | `tinntester-index validate --strict --json` |
| `dhf-tracker validate` | IEC 62304 traceability validation | `python dhf-tracker/scripts/validate-traceability.py` |
| `ast-grep` | Structural code search (faster than grep for patterns) | MCP tool or `sg scan --rule <rule>` |
| `compile_commands.json` | Pre-built file index for C++ | At `apps/stm32_tinntester/compile_commands.json` |

### Minimize File Reads

- Use `Grep` with specific patterns before reading whole files
- Use `Glob` to find files by pattern before exploring directories
- Read only the sections you need (use `offset` and `limit` parameters)
- If a file was already analyzed in the journal (`.agent-bus/messages.jsonl`), don't re-analyze it

## MCP Tool Names (When Agent-Bus Is Loaded as MCP Server)

If agent-bus is registered in your MCP config (Claude Code, Codex, Gemini), these tools are available directly:

| MCP Tool | Purpose | Key Parameters |
|----------|---------|----------------|
| `bus_health` | Check Redis + PG status | (none) |
| `post_message` | Send a message | `sender`, `recipient`, `topic`, `body`, `tags[]`, `schema`, `thread_id`, `priority`, `request_ack` |
| `list_messages` | Read messages | `agent` (recipient filter), `sender`, `since_minutes`, `limit`, `include_broadcast` |
| `ack_message` | Acknowledge a message | `agent`, `message_id`, `body` |
| `set_presence` | Register agent availability | `agent`, `status`, `capabilities[]`, `ttl_seconds`, `metadata` |
| `list_presence` | List all active agents | (none) |
| `list_presence_history` | PG presence audit trail | `agent`, `since_minutes`, `limit` |

**Schema validation is enforced on `post_message`** when the `schema` parameter is provided.

### MCP Usage Example (in-session, no HTTP/CLI needed)

```
Use post_message with:
  sender: "my-agent-id"
  recipient: "claude"
  topic: "analysis-findings"
  body: "FINDING: Issue found\nSEVERITY: HIGH\nFILE: src/main.rs:42"
  tags: ["repo:my-project", "severity:high"]
  schema: "finding"
```

## Orchestration Patterns

### Pattern 1: Parallel Analysis (Read-Only)

```
Orchestrator:
  1. bus_health → verify infrastructure
  2. set_presence → announce session with capabilities
  3. post_message → "Starting analysis of <repo>. Dispatching N agents."
  4. Dispatch N specialist agents with bus instructions
  5. Monitor: list_messages(agent=claude, since_minutes=5)
  6. When all agents post COMPLETE → synthesize findings
  7. post_message → session benchmark (schema: benchmark)

Each Agent:
  1. post_message → "Online and starting <task>" (schema: status)
  2. Analyze files, post each finding (schema: finding)
  3. Every 2-3 tool calls: list_messages(agent=<my-id>) → check for follow-ups
  4. post_message → "COMPLETE: N findings" (schema: finding)
```

### Pattern 2: Chained Task Assignment

```
Orchestrator:
  1. Dispatch Agent A for initial analysis
  2. Read Agent A's findings from bus
  3. post_message(recipient=Agent B, topic="follow-up-task") based on A's findings
  4. Agent B reads task, continues deeper analysis
  5. Orchestrator synthesizes A + B findings
```

### Pattern 3: Cross-Repo Coordination

```
Orchestrator working on repo-X:
  1. Tag all messages: tags=["repo:repo-X"]
  2. Another session on repo-Y uses tags=["repo:repo-Y"]
  3. Messages don't conflict — filtered by tag
  4. Journal export per repo: agent-bus journal --tag "repo:repo-X" --output .agent-bus/messages.jsonl
```

### Pattern 4: Session Recovery

```
New session on same repo:
  1. Read journal: .agent-bus/messages.jsonl
  2. list_messages(since_minutes=1440) → last 24h of coordination
  3. Avoid re-analyzing files already covered by previous agents
  4. Post status: "Resuming from previous session. Prior findings: N"
```

## Transport Modes

| Mode | Latency | Use Case |
|------|---------|----------|
| **MCP tools** (stdio) | <100ms | **Best when agent-bus is loaded as MCP server.** Direct tool calls. |
| **HTTP POST** (`localhost:8400`) | <50ms | **Default for subagents.** No MCP session needed. |
| **CLI** (`agent-bus send`) | ~2.5s | Scripts, one-off ops, environments without curl/MCP |

**Priority**: MCP tools > HTTP POST > CLI. Use the fastest available.

## Endpoint Reference

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/health` | Bus health with Redis + PG stats |
| `POST` | `/messages` | Send a message (JSON body) |
| `GET` | `/messages?agent=X&since=N&limit=N` | Read messages for an agent |
| `POST` | `/messages/:id/ack` | Acknowledge a message |
| `GET` | `/events?agent=X&broadcast=bool` | SSE live stream |
| `PUT` | `/presence/:agent` | Set agent presence |
| `GET` | `/presence` | List all active agents |
| `GET` | `/presence/history?agent=X&since=N` | PG presence audit trail |

## CLI Quick Reference

```bash
agent-bus health --encoding json              # Full status
agent-bus send --from-agent X --to-agent Y --topic T --body B --schema finding
agent-bus read --agent X --since-minutes 60 --encoding human
agent-bus watch --agent X --history 20        # Real-time streaming
agent-bus journal --tag "repo:X" --output .agent-bus/messages.jsonl
agent-bus sync                                # Backfill Redis → PG
agent-bus prune --older-than-days 30          # PG retention
agent-bus export --since-minutes 1440         # NDJSON full dump
agent-bus presence --agent X --capability mcp
agent-bus presence-list
agent-bus presence-history --agent X
```

## Stable Agent IDs

`codex`, `claude`, `gemini`, `copilot`, `euler`, `pasteur`, `all`

Specialist agents use descriptive IDs: `dedup-auditor`, `uvc-analyst`, `rust-reviewer`, `quality-reviewer`, `iec62304-reviewer`, `cicd-engineer`, `req-tagger`, etc.

## Tags Convention

Always include: `repo:<repo-name>`
Severity: `severity:critical|high|medium|low`
Session: `session:<session-name>`
Type: `type:benchmark|validation|coordination`
Status: `status:complete|fixed|verified`
