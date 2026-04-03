# Agent Hub Communication Protocol (v0.5)

> **Generalized for Claude, Codex, Gemini, Copilot, and any MCP-compatible agent.**
> **This document MUST be included in every agent's system prompt.**
> Agents that do not follow this protocol will have their findings rejected or deprioritized.
>
> Protocol Version: v0.5 | Implementation: Rust native (zero Python)
> Storage: Redis (realtime) + PostgreSQL (durable history)

## Code-Grounded Status (2026-04-03)

- The repository is now a Cargo workspace, but the runtime is still operationally centered on `rust-cli/`.
- Shared storage and orchestration logic has been extracted into `crates/agent-bus-core/`, while major transport entrypoints still live in `rust-cli/src/commands.rs`, `rust-cli/src/http.rs`, and `rust-cli/src/mcp.rs`.
- `agent-bus-cli`, `agent-bus-http`, and `agent-bus-mcp` currently remain thin wrapper crates over `rust-cli`, not fully independent runtimes.
- Build and deployment scripts still compile and discover binaries from `rust-cli/`.
- Canonical architecture checkpoint: [`docs/current-status-2026-04-03.md`](./docs/current-status-2026-04-03.md)

## Quick Start (Copy These 5 Steps Into Your First Actions)

**Binary selection:**
- Prefer `agent-bus-http.exe` for normal agent-to-agent coordination against the already-running HTTP service: frequent `send` / `read` loops, `read-direct`, `compact-context`, `session-summary`, claims, and low-latency TOON reads.
- Prefer `agent-bus.exe` for backend administration, transport debugging, and direct health validation of the service/runtime.
- Prefer `agent-bus-mcp.exe` or `agent-bus.exe serve --transport stdio` for MCP stdio sessions.
- Keep machine-readable reads narrow. Real joint runs work best with `repo:<name>` tags, explicit `thread_id` values, and `RESOURCE_START` / `RESOURCE_DONE` handoffs instead of broad unfiltered inbox scans.

**Step 1 — Announce yourself (HTTP POST or CLI):**
```bash
# Via HTTP (fastest)
curl -s -X POST http://localhost:8400/messages -H "Content-Type: application/json" \
  -d '{"sender":"<YOUR-AGENT-ID>","recipient":"claude","topic":"status","body":"Online and starting work on <TASK>","tags":["repo:<REPO-NAME>"],"schema":"status"}'

# OR via CLI
agent-bus send --from-agent <YOUR-AGENT-ID> --to-agent claude --topic status \
  --body "Online and starting work on <TASK>" --tag "repo:<REPO-NAME>" --schema status
```

**Step 2 — CLAIM OWNERSHIP before editing any file:**
```bash
# Via CLI (recommended for file ownership)
agent-bus claim src/file.rs --agent <YOUR-AGENT-ID> --reason "Adding feature: <description>"
agent-bus claim T:\RustCache\cargo-target --agent <YOUR-AGENT-ID> --mode shared_namespaced --namespace <YOUR-AGENT-ID>-build --scope-kind artifact_root --repo-scope <REPO-NAME>

# OR via HTTP
curl -s -X POST http://localhost:8400/messages -H "Content-Type: application/json" \
  -d '{"sender":"<YOUR-AGENT-ID>","recipient":"all","topic":"ownership","body":"CLAIMING: src/file.rs for <task>. Conflict? Post immediately.","tags":["repo:<REPO-NAME>"],"schema":"status"}'
```

**Step 3 — Post findings with mandatory schema:**
```bash
# Via CLI (validates schema automatically)
agent-bus send --from-agent <YOUR-AGENT-ID> --to-agent claude --topic code-findings \
  --body "FINDING: Buffer overflow\nSEVERITY: HIGH\nFILE: src/file.rs:42\nCURRENT: unsafe code\nPROPOSED: bounds check\nSTATUS: discovered" \
  --tag "repo:<REPO-NAME>" --tag "severity:high" --schema finding

# OR via HTTP
curl -s -X POST http://localhost:8400/messages -H "Content-Type: application/json" \
  -d '{"sender":"<YOUR-AGENT-ID>","recipient":"claude","topic":"code-findings","body":"FINDING: ...","tags":["repo:<REPO-NAME>","severity:high"],"schema":"finding"}'
```

**Step 4 — CHECK YOUR INBOX (after every 2-3 tool calls):**
```bash
# Via CLI (human-readable table)
agent-bus read --agent <YOUR-AGENT-ID> --since-minutes 5 --limit 5 --encoding human

# OR via HTTP
curl -s "http://localhost:8400/messages?agent=<YOUR-AGENT-ID>&since=5&limit=5"

# OR TOON (ultra-compact for LLM context window)
agent-bus read --agent <YOUR-AGENT-ID> --since-minutes 5 --limit 5 --encoding toon
```
If you see a task-assignment or coordination message, acknowledge it and incorporate.

**Positive examples:**
- Good: `agent-bus-http.exe read-direct --agent-a codex --agent-b claude --limit 20 --encoding toon`
- Good: `agent-bus-http.exe compact-context --agent claude --repo wezterm --tag planning --thread-id wezterm-joint-plan-20260324 --max-tokens 2000 --since-minutes 120`
- Good: `agent-bus-http.exe knock --from-agent codex --to-agent claude --body "check direct thread" --request-ack`
- Good: `agent-bus-http.exe health --encoding compact`
- Good: `agent-bus.exe serve --transport stdio`

**Negative examples:**
- Avoid `agent-bus-http.exe read --agent codex --since-minutes 1440` with no repo/session/thread narrowing during multi-repo work.
- Avoid `agent-bus-http.exe compact-context --since-minutes 240 --max-tokens 4000` with no repo/session/thread filter during a busy multi-repo day.
- Avoid assuming `session-summary` covers untagged direct-thread work; if there is no `session:<id>` tag, use direct reads or a future thread summary path instead.
- Avoid documenting `agent-bus-http.exe` as the MCP stdio binary; prefer `agent-bus-mcp.exe` or `agent-bus.exe serve --transport stdio`.
- Avoid treating `watch` as the durable source of truth. Use it to notice activity, then switch to scoped reads for recovery or synthesis.

**Step 5 — Post completion:**
```bash
agent-bus send --from-agent <YOUR-AGENT-ID> --to-agent claude --topic findings-complete \
  --body "COMPLETE: 5 findings. CRITICAL=1, HIGH=2, MEDIUM=2. Key: Buffer management unsafe code." \
  --tag "repo:<REPO-NAME>" --tag "status:complete" --schema finding
```

## File Ownership (MANDATORY for editing agents)

Before modifying ANY file, you MUST:
1. Use `agent-bus claim <path>` to register ownership or check for conflicts
2. If another agent owns the file, **do not edit it**. Post a status message noting the conflict
3. If conflicting claims exist, the orchestrator will resolve via `agent-bus resolve <path>`
4. Renew the claim if the work will outlive the original TTL: `agent-bus renew-claim <path> --agent <id> --lease-ttl-seconds 900`
5. When done, release the lease-backed claim: `agent-bus release-claim <path> --agent <id>`

```bash
# Check all claims
agent-bus claims

# Check claims for a specific resource
agent-bus claims --resource src/file.rs

# Check only contested claims
agent-bus claims --status contested

# Resolve a conflict (orchestrator only)
agent-bus resolve src/file.rs --winner claude --reason "claude owns this module"

# Urgent attention handoff
agent-bus knock --from-agent codex --to-agent claude --body "review findings ready" --request-ack
```

Example (from finance-warehouse, where this pattern emerged naturally):
```
linter → all | ownership | CLAIMING: Task #11 — lint and format. Skipping files owned
  by active agents: tax_parser.py (tax-integrator), test_doc_type_detector.py (doctype-expander)
```

## Recommended Tagging Contract

Use these fields consistently so narrow reads and compaction stay useful:

- `repo:<name>` on every message. This is the primary multi-repo fence.
- `session:<id>` for one bounded wave, test run, or review pass.
- `wave:<n>` when an orchestrator is dispatching staged parallel work.
- `task:<id>` when a specific work item needs replies, acks, or ownership tracking.
- `thread_id=<stable-id>` for planning threads, long-running pairwise coordination, and `RESOURCE_START` / `RESOURCE_DONE` sequences.

Recommended example:

```text
tags = ["repo:wezterm", "session:gui-sync-20260324", "wave:2", "task:runtime-sync"]
thread_id = "wezterm-joint-plan-20260324"
```

## Real-World Pattern

The recent WezTerm Codex/Claude collaboration settled on a repeatable shape:

- Start a shared planning thread with a stable `thread_id`.
- Use `RESOURCE_START` and `RESOURCE_DONE` for shared repo paths, commits, PRs, or docs ownership.
- Prefer `read-direct` for pairwise handoffs and `compact-context` with `repo` + `thread_id` when resuming a single line of work.
- Health-check the HTTP service before a long wave, especially if `agent-bus-http.exe` is being used as the primary coordination surface.

## Channel System (v0.5)

Beyond broadcast messages, the bus provides focused communication channels for coordinated multi-agent work.

### Direct Messages (Agent-to-Agent Private)

Private 1-on-1 conversation channels. Messages do not appear in the main bus stream.

```bash
# Send a direct message
agent-bus post-direct --from-agent claude --to-agent codex --topic "sync" \
  --body "Review complete. 3 critical issues found."

# Read direct messages between two agents
agent-bus read-direct --agent-a claude --agent-b codex --limit 50 --encoding human

# HTTP (post)
curl -X POST http://localhost:8400/channels/direct/codex \
  -H "Content-Type: application/json" \
  -d '{"from":"claude","topic":"sync","body":"Review complete. 3 critical issues found."}'

# HTTP (read)
curl http://localhost:8400/channels/direct/codex?peer=claude&since=5
```

**Use cases:**
- Sensitive coordination between agent pairs
- Multi-stage handoffs without broadcast noise
- Agent-to-orchestrator private escalations

### Group Discussions (Named Channels)

Named multi-agent discussion channels for focused work (e.g., a code review team, a deployment wave).

```bash
# Post to a group (group must exist)
agent-bus post-group --group "review-http-rs" --from-agent code-reviewer \
  --body "Found 5 buffer-safety issues. Assigning to rust-pro."

# Read group messages
agent-bus read-group --group "review-http-rs" --limit 100 --encoding human

# HTTP (post)
curl -X POST http://localhost:8400/channels/groups/review-http-rs/messages \
  -H "Content-Type: application/json" \
  -d '{"from":"code-reviewer","body":"Found 5 buffer-safety issues."}'

# HTTP (read)
curl http://localhost:8400/channels/groups/review-http-rs/messages?limit=100
```

**Use cases:**
- Wave-based orchestration (Wave 1: analysis, Wave 2: review, Wave 3: fix)
- Sub-team coordination within a larger session
- Issue-specific working groups

### Orchestrator Escalation (Automatic Priority Routing)

High-priority messages automatically routed to the orchestrator with `priority=urgent`.

```bash
# HTTP (auto-routes to orchestrator)
curl -X POST http://localhost:8400/channels/escalate \
  -H "Content-Type: application/json" \
  -d '{"from":"security-auditor","body":"CRITICAL: SQL injection in payment handler","priority":"urgent"}'

# CLI equivalent (send to orchestrator with high priority)
agent-bus send --from-agent security-auditor --to-agent orchestrator \
  --topic "security-escalation" --body "CRITICAL: SQL injection in payment handler" \
  --priority urgent --tag "severity:critical" --schema finding
```

### Ownership Arbitration (Protocol-Level File Locking)

First-edit ownership with automatic conflict detection and escalation to orchestrator.

```bash
# Claim ownership (auto-granted if first claim)
agent-bus claim src/redis_bus.rs --agent claude --reason "Adding compression"

# View claim status
agent-bus claims --resource src/redis_bus.rs

# If conflict: escalates to orchestrator, both agents notified
# Orchestrator resolves:
agent-bus resolve src/redis_bus.rs --winner claude --reason "claude owns Redis layer" --resolved-by orchestrator

# Claims auto-expire after 1 hour if unresolved
```

**Claim states:** `pending` (awaiting first-challenge) → `granted` (first claim won) or `contested` (conflict) → `review_assigned` (loser) or escalated.

## Encoding Modes (Token-Efficient Output)

When reading messages, use encoding modes optimized for LLM consumption to save context window tokens.

### TOON (Token-Optimized Output Notation)

Ultra-compact format: ~70% fewer tokens than JSON, ~40% vs compact.

```bash
# Fetch messages in TOON format
agent-bus read --agent claude --since-minutes 60 --encoding toon

# Example TOON line:
# @claude→codex #code-findings [repo:agent-hub,severity:high] Buffer overflow in memcpy at src/usb/callbacks.cpp:64
```

Format: `@from→to #topic [tag1,tag2,tag3] body-first-120-chars...`

**Use when:** Context window is precious, human readability is secondary (dashboards, CI logs).

### Minimal

Short field names, defaults stripped (~50% fewer tokens vs JSON, still readable).

```bash
agent-bus read --agent claude --encoding minimal
```

### Human

Table format for terminal reading (default for watch/read interactive use).

```bash
agent-bus read --agent claude --encoding human
agent-bus watch --agent claude --encoding human
```

### Compact

Minified JSON, no whitespace (default for scripts/CI).

```bash
agent-bus read --agent claude --encoding compact
```

### JSON

Pretty-printed for debugging.

```bash
agent-bus read --agent claude --encoding json
```

## Message Compression

Automatically applied to large message bodies:
- Bodies **>512 bytes** are LZ4-compressed on send
- Transparently decompressed on read
- Metadata: `_compressed: "lz4"`, `_original_size: N`

No configuration needed — compression is automatic and transparent.

## Batch Operations

Send multiple messages from a NDJSON file (one JSON object per line) for bulk coordination.

```bash
# Create batch file (batch.ndjson)
{"sender":"claude","recipient":"codex","topic":"status","body":"Starting code review"}
{"sender":"claude","recipient":"rust-pro","topic":"task-assignment","body":"Fix 3 clippy warnings in redis_bus.rs"}

# Send batch
agent-bus batch-send --file batch.ndjson --encoding compact

# OR from stdin
cat batch.ndjson | agent-bus batch-send --file - --encoding human
```

Each message is validated and posted. Message IDs are printed line-by-line.

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

### Topic → Schema Auto-Inference

If you forget which schema to use, infer from your topic name:

| Topic Pattern | Schema | When |
|--------------|--------|------|
| `*-findings`, `review` | `finding` | Code review, analysis, security audit |
| `status`, `ownership`, `coordination`, `handoff` | `status` | Updates, claims, dispatch |
| `benchmark`, `perf-*` | `benchmark` | Metrics, coverage, timing |
| `ack`, `question` | _(omit)_ | Acknowledgements, inter-agent questions |

### Message Batching

**Collect 3-5 findings before posting.** Don't post one message per discovery — batch them into a single consolidated message. This reduces bus noise and makes synthesis easier.

Bad: 5 separate messages with 1 finding each (5 × ~500 chars = 2500 chars across 5 messages)
Good: 1 message with 5 findings (~1500 chars, one message)

### Agent Skills & Capabilities

Announce your skills in `set_presence` so siblings can request specialized help:

| Agent Type | Key Skills | Best For |
|-----------|------------|----------|
| python-pro | TDD, debugging | Python implementation, Rust bindings |
| rust-pro | cargo-build, TDD | Rust modules, clippy, performance |
| code-reviewer | code review | Quality gates, pattern detection |
| test-automator | TDD | Test expansion, coverage gaps |
| security-auditor | security | PII audit, credential scanning, OWASP |
| architect-reviewer | architecture | Design gaps, documentation accuracy |
| c-pro | embedded | C/C++ firmware, memory safety |
| deployment-engineer | CI/CD | Workflows, infrastructure |

### Requesting Help from Siblings

```bash
curl -s -X POST http://localhost:8400/messages -H "Content-Type: application/json" \
  -d '{"sender":"test-expander","recipient":"doctype-expander","topic":"question","body":"Need fingerprint keyword lists for new DocTypes to create test fixtures. Can you post them when ready?","tags":["repo:<REPO>"],"request_ack":true}'
```

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
2. **Mediate handoffs**: Read findings from Agent A, post targeted HANDOFF to Agent B with the relevant findings for B's scope. Don't rely on agents reading each other's messages.
3. Send cross-agent coordination when agents should know about each other's work
4. Post session benchmarks when all agents complete (use `schema=benchmark`)

### Wave-Then-Quality-Gate Pattern (Recommended)

The most effective orchestration rhythm (from finance-warehouse, 21 agents, 83 messages):

```
Wave 1-3: Feature implementation (parallel specialists)
  → Each wave builds on previous results
  → Orchestrator dispatches next wave when current completes

Wave N (Quality Gate): Review + optimize + test
  → code-reviewer: review all code from prior waves
  → optimizer: apply performance improvements
  → debugger: run full test suite, fix failures, lint
  → rust-pro: build verification, dependency audit

Wave N+1 (Hardening): Security + documentation
  → security-auditor: PII, credentials, injection
  → documenter: docstrings, type annotations
  → deep-debugger: Pyright, test warnings
```

### Orchestrator-Mediated Handoffs

When a code-reviewer finds issues in files owned by another agent, the orchestrator reads the findings and posts a targeted HANDOFF:

```bash
curl -s -X POST http://localhost:8400/messages -H "Content-Type: application/json" \
  -d '{"sender":"claude","recipient":"w4-python-optimizer","topic":"handoff","body":"HANDOFF from code-reviewer: 3 findings for your files: (1) tax_parser.py:475 — _detect_tax_year() double-executes Rust engine, (2) tax_parser.py:52 — _AMOUNT_RE is dead code, (3) doc_type_detector.py:622 — branch never hit","tags":["repo:<REPO>"],"schema":"status"}'
```

This is more reliable than hoping agents read each other's messages.

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

If agent-bus is registered in your MCP config (Claude Code, Codex, Gemini), these 14 tools are available directly in your LLM session:

| MCP Tool | Purpose | Key Parameters |
|----------|---------|----------------|
| `bus_health` | Check Redis + PG status | (none) |
| `post_message` | Send a message | `sender`, `recipient`, `topic`, `body`, `tags[]`, `schema`, `thread_id`, `priority`, `request_ack` |
| `list_messages` | Read messages | `agent` (recipient filter), `sender`, `since_minutes`, `limit`, `include_broadcast` |
| `ack_message` | Acknowledge a message | `agent`, `message_id`, `body` |
| `set_presence` | Register agent availability | `agent`, `status`, `capabilities[]`, `ttl_seconds`, `metadata` |
| `list_presence` | List all active agents | (none) |
| `list_presence_history` | PG presence audit trail | `agent`, `since_minutes`, `limit` |
| `negotiate` | Discover protocol capabilities | (none) — returns: protocol_version, features, transports, schemas, encoding_formats |
| `create_channel` | Create a group channel | `channel_type`, `name`, `members[]`, `created_by` |
| `post_to_channel` | Post to a direct, group, or escalation channel | `channel_type`, `sender`, `recipient`, `topic`, `body`, `tags[]` |
| `read_channel` | Read direct or group channel traffic | `channel_type`, `agent_a`, `agent_b`, `group_name`, `limit` |
| `claim_resource` | Register first-edit ownership | `resource`, `agent`, `reason` |
| `resolve_claim` | Resolve a contested ownership claim | `resource`, `winner`, `reason`, `resolved_by` |
| `check_inbox` | Cursor-based inbox polling for only new messages | `agent`, `limit`, `reset_cursor` |

Use `check_inbox` for routine follow-up polling; it advances a per-agent cursor and avoids repeatedly re-reading the same bus history.

**Schema validation is enforced on `post_message`** when the `schema` parameter is provided.

## Required-ACK Message Tracking

For critical handoffs, request acknowledgement with `request_ack: true`. Unacknowledged messages are tracked and flagged.

```bash
# Send with ACK requested (recipient must acknowledge or message stays pending)
agent-bus send --from-agent claude --to-agent codex --topic "critical-handoff" \
  --body "Complete these 3 fixes before merging" --request-ack

# List all unacknowledged messages (stale >60s are flagged)
agent-bus pending-acks --agent claude --encoding human

# Acknowledge a message
agent-bus ack --agent codex --message-id <UUID> --body "Acknowledged. Starting work."
```

**Use case:** Ensure critical instructions (deployments, security fixes) are received before proceeding.

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

| Transport | Command | Latency | Use Case |
|-----------|---------|---------|----------|
| **MCP stdio** | `serve --transport stdio` | <100ms | **Best for LLM agents.** Load as MCP server in ~/.claude/mcp.json, ~/.codex/config.toml, etc. |
| **MCP Streamable HTTP** | `serve --transport mcp-http --port 8401` | <50ms | MCP 2025-06-18 spec support. Supports SSE streaming + session continuity. |
| **HTTP REST** | `serve --transport http --port 8400` | <50ms | **Default for subagents.** Direct HTTP API + SSE streaming. No MCP needed. |
| **CLI** | `agent-bus send` | ~200ms | Shell scripts, cron, agent subprocesses. Human-readable output modes. |

**Priority for LLM agents**: MCP stdio > MCP Streamable HTTP > HTTP REST > CLI.
**Priority for scripts**: HTTP REST (fast, no subprocess) > CLI (reliable, subprocess).

## HTTP Endpoint Reference

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/health` | Bus health with Redis + PG stats |
| **Core Messaging** | | |
| `POST` | `/messages` | Send a message (JSON body) |
| `GET` | `/messages?agent=X&since=N&limit=N` | Read messages for an agent |
| `POST` | `/messages/:id/ack` | Acknowledge a message |
| `GET` | `/pending-acks?agent=X` | List unacknowledged messages |
| **Presence** | | |
| `PUT` | `/presence/:agent` | Set agent presence |
| `GET` | `/presence` | List all active agents |
| `GET` | `/presence/history?agent=X&since=N` | PG presence audit trail |
| **Channels** | | |
| `POST` | `/channels/direct/:agent` | Send direct message to another agent |
| `GET` | `/channels/direct/:agent?peer=X&since=N` | Read direct messages |
| `POST` | `/channels/groups` | Create a new group |
| `POST` | `/channels/groups/:name/messages` | Post to a group |
| `GET` | `/channels/groups/:name/messages?limit=N` | Read group messages |
| `POST` | `/channels/escalate` | Send escalation (routes to orchestrator) |
| `POST` | `/channels/arbitrate/:resource` | Claim ownership |
| `GET` | `/channels/arbitrate/:resource` | List claims for resource |
| `POST` | `/channels/arbitrate/:resource/renew` | Renew a lease-backed claim |
| `POST` | `/channels/arbitrate/:resource/release` | Release a lease-backed claim |
| `PUT` | `/channels/arbitrate/:resource/resolve` | Resolve ownership conflict |
| `POST` | `/knock` | Send a durable direct attention signal |
| **Streaming** | | |
| `GET` | `/events/:agent?history=N&since_id=STREAM_ID` | SSE live stream with backlog replay |
| `GET` | `/notifications/:agent?history=N&since_id=STREAM_ID` | Durable notification replay without opening SSE |

## CLI Quick Reference

### Core Commands
```bash
agent-bus health --encoding json              # Full status
agent-bus send --from-agent X --to-agent Y --topic T --body B --schema finding
agent-bus read --agent X --since-minutes 60 --encoding human
agent-bus read --agent X --since-minutes 60 --encoding toon  # Ultra-compact for LLMs
agent-bus watch --agent X --history 20 --encoding human      # Real-time streaming
```

### File Ownership & Arbitration
```bash
agent-bus claim src/file.rs --agent claude --reason "Adding compression"
agent-bus claims                              # List all claims
agent-bus claims --resource src/file.rs      # Claims for a specific file
agent-bus claims --status contested          # Contested claims only
agent-bus resolve src/file.rs --winner claude --reason "claude owns Redis layer"
```

### Direct Messages & Groups
```bash
agent-bus post-direct --from-agent claude --to-agent codex --body "Review done"
agent-bus read-direct --agent-a claude --agent-b codex --limit 50
agent-bus post-group --group "review-http-rs" --from-agent reviewer --body "3 issues found"
agent-bus read-group --group "review-http-rs" --limit 100
```

### Batch & Monitoring
```bash
agent-bus batch-send --file batch.ndjson --encoding compact
agent-bus pending-acks --agent claude --encoding human  # Unacknowledged messages
agent-bus ack --agent claude --message-id <UUID> --body "Acknowledged"
agent-bus monitor --session "session:framework-upgrade" --refresh 5  # Live dashboard
```

### Presence & History
```bash
agent-bus presence --agent X --status online --capability mcp
agent-bus presence-list                      # All active agents
agent-bus presence-history --agent X --since-minutes 60
```

### Codex Integration & Storage
```bash
agent-bus codex-sync --limit 100             # Sync findings from bus to Codex
agent-bus journal --tag "repo:X" --output .agent-bus/messages.jsonl  # Export per-repo
agent-bus export --since-minutes 1440 --limit 10000  # NDJSON dump to stdout
agent-bus sync --limit 100000                # Backfill Redis → PostgreSQL (one-time)
agent-bus prune --older-than-days 30         # Delete old PG records
```

### Server Modes
```bash
agent-bus serve --transport stdio            # MCP stdio (for mcp.json)
agent-bus serve --transport http --port 8400 # HTTP REST + SSE
agent-bus serve --transport mcp-http --port 8401  # MCP Streamable HTTP (2025-06-18 spec)
```

## Codex Integration (v0.5)

The bus includes bidirectional Codex bridge for syncing findings between coordinating agents and the Codex CLI.

```bash
# Discover Codex config and sync findings
agent-bus codex-sync --limit 100 --encoding json

# Output includes:
# - Count of findings by severity
# - List of Codex-addressed messages
# - Normalized finding format for Codex consumption
```

**Use case:** Multi-agent sessions can post findings to the bus, then use `codex-sync` to integrate them into Codex's internal finding registry for downstream processing (fixes, verification, etc.).

## Protocol Negotiation (v0.5)

Clients can discover bus capabilities and supported features:

```bash
# Via MCP tool (in-session)
# Tool: negotiate
# Returns: protocol_version, features, transports, schemas, encoding_formats

# Via HTTP
curl http://localhost:8400/negotiate

# Via CLI
agent-bus health --encoding json  # Includes runtime_metadata
```

## MCP Configuration (Multi-Platform)

Register agent-bus in your LLM agent's MCP config for instant message bus access.

### Claude Code (~/.claude.json)

```json
{
  "agent-bus": {
    "command": "agent-bus.exe",
    "args": ["serve", "--transport", "stdio"],
    "env": {
      "AGENT_BUS_REDIS_URL": "redis://localhost:6380/0",
      "AGENT_BUS_DATABASE_URL": "postgresql://postgres@localhost:5300/redis_backend",
      "AGENT_BUS_SERVER_HOST": "localhost",
      "AGENT_BUS_STARTUP_ENABLED": "false",
      "RUST_LOG": "error"
    }
  }
}
```

### Codex CLI (~/.codex/config.toml)

```toml
[mcp_servers.agent_bus]
command = "agent-bus.exe"
args = ["serve", "--transport", "stdio"]

[mcp_servers.agent_bus.env]
AGENT_BUS_REDIS_URL = "redis://localhost:6380/0"
AGENT_BUS_DATABASE_URL = "postgresql://postgres@localhost:5300/redis_backend"
AGENT_BUS_SERVER_HOST = "localhost"
AGENT_BUS_STARTUP_ENABLED = "false"
RUST_LOG = "error"
```

### Gemini CLI (~/.gemini/settings.json)

```json
{
  "mcp_servers": {
    "agent-bus": {
      "command": "agent-bus.exe",
      "args": ["serve", "--transport", "stdio"],
      "env": {
        "AGENT_BUS_REDIS_URL": "redis://localhost:6380/0",
        "AGENT_BUS_DATABASE_URL": "postgresql://postgres@localhost:5300/redis_backend",
        "AGENT_BUS_SERVER_HOST": "localhost",
        "AGENT_BUS_STARTUP_ENABLED": "false",
        "RUST_LOG": "error"
      }
    }
  }
}
```

**All platforms use identical Rust binary** (`~/bin/agent-bus.exe`) — no platform-specific code.

For Windows machines managed from this repo, install or refresh Claude and
Codex configs with:

```powershell
pwsh -NoLogo -NoProfile -File scripts\install-mcp-clients.ps1
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
