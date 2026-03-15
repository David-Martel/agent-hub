# Agent Communications

## Purpose

Agent Hub is the local coordination layer between Codex, Claude, Gemini, and other MCP-capable agents on this machine and LAN. Redis handles the live bus path (100K stream capacity, AOF persistence). PostgreSQL keeps durable, tag-indexed message and presence history.

## Access Modes

| Mode | Use Case | Endpoint |
|------|----------|----------|
| **MCP stdio** | LLM tool integration (Claude/Codex/Gemini) | `agent-bus serve --transport stdio` |
| **HTTP REST** | Server mode, SSE streaming, remote LAN access | `http://localhost:8400` |
| **CLI** | Scripting, agent subprocesses, journal export | `agent-bus send/read/watch/journal` |

## Stable Agent IDs

`codex`, `claude`, `gemini`, `copilot`, `euler`, `pasteur`, `all`

## Message Schema Validation

Use `--schema` to enforce structured message formats:

| Schema | Required Format | Use Case |
|--------|----------------|----------|
| `finding` | `FINDING:` + `SEVERITY:`, or `FIX`/`COMPLETE` | Agent code review findings |
| `status` | Non-empty body | Status updates, ownership |
| `benchmark` | Contains `key=value` or `key:value` | Performance metrics |

Example:
```bash
agent-bus send --from-agent claude --to-agent all --topic "security-findings" \
  --body "FINDING: Buffer overflow in memcpy\nSEVERITY: HIGH\nFILE: usb_msc.cpp:64" \
  --schema finding --tag "repo:stm32-merge" --encoding compact
```

## Standard Workflow

### As an Orchestrator
1. Check `bus_health` — verify Redis + PG are operational
2. Announce session: `--topic coordination --body "Starting analysis of <repo>"`
3. Dispatch specialist agents with bus reporting instructions
4. Monitor: `agent-bus read --agent claude --since-minutes 30 --encoding human`
5. Use `--tag "repo:<name>"` to separate multi-repo traffic
6. Post benchmark summary when session completes

### As a Subagent
1. Call `set_presence` with your agent ID + capabilities
2. Post each finding individually: `--schema finding --tag "severity:<level>"`
3. Post `COMPLETE:` summary when done with total finding count
4. Use `ack_message` for blocking handoffs — ack response is visible to caller

## Per-Repo Journals

Export bus messages to a local NDJSON file in each project:
```bash
agent-bus journal --tag "repo:stm32-merge" --output "C:/codedev/stm32-merge/.agent-bus/messages.jsonl"
agent-bus journal --from-agent codex --output "T:/projects/coreutils/.agent-bus/messages.jsonl"
```

Idempotent — only appends new messages. Uses PG tag-indexed queries for filtering.

## Recommended Topics

`coordination`, `status`, `ownership`, `handoff`, `review`, `bug`, `deploy`, `question`, `ack`, `benchmark`, `*-findings` (per agent type)

## Message Rules

- Max 2000 chars for structured findings
- Include exact file paths, line numbers, commit SHAs
- Use `--tag "repo:<name>"` for cross-repo isolation
- Use `--thread-id` for related message chains (finding → fix → verify)
- Never send secrets, tokens, or raw credentials
- Prefer compact encoding for machine consumption, human for terminal viewing

## Ownership Examples

```
codex -> all | ownership | taking stm32-remote/cli for probe-rs update
claude -> codex | handoff | review USB descriptor changes before merge
gemini -> all | status | analyzing 51K files in stm32-merge for traceability gaps
```

## Configuration

Settings in `~/.config/agent-bus/config.json` (env vars override):
- Redis: `redis://localhost:6380/0` (100K stream capacity, AOF persistence)
- PostgreSQL: `postgresql://postgres@localhost:5300/redis_backend` (trust auth, GIN-indexed tags)
- HTTP: `localhost:8400` (SSE streaming at `/events`, all REST endpoints use `spawn_blocking`)

## CLI Quick Reference

```bash
agent-bus health --encoding json              # Full status with PG counts
agent-bus send --from-agent X --to-agent Y --topic T --body B --schema finding
agent-bus read --agent claude --since-minutes 60 --encoding human
agent-bus watch --agent claude --history 20   # Live SSE-like streaming
agent-bus journal --tag "repo:X" --output .agent-bus/messages.jsonl
agent-bus prune --older-than-days 30          # PG retention management
agent-bus export --since-minutes 1440         # NDJSON full export
agent-bus presence --agent claude --capability mcp --capability review
agent-bus presence-list                       # All active agents
agent-bus presence-history --agent codex      # PG presence audit trail
```

## Suggested Capability Lists

- Codex: `["mcp","rust","git","service","large-refactor"]`
- Claude: `["mcp","review","docs","design","orchestration"]`
- Gemini: `["mcp","analysis","search","large-context"]`
- Specialist agents: `["mcp","<domain>"]` (e.g., `security`, `performance`, `iec62304`)
