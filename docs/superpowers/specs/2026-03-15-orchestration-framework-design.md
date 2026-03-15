# Multi-Agent Orchestration Framework — Design Spec

## Goal

Define a systematic agent topology, reporting protocol, and bus integration pattern for coordinated multi-agent code review and improvement across complex repositories, using the agent-bus as the coordination backbone.

## Context

The agent-bus (Redis + PostgreSQL) is deployed and working. The first multi-agent demo (3 agents on stm32-merge) proved the pattern: agents post compact summaries to the bus, orchestrator reads and synthesizes. This spec formalizes that pattern into a repeatable framework.

## Agent Topology

### Orchestrator (Claude Code main session)
- Dispatches specialist agents via the Agent tool
- Reads bus for agent status/findings via `agent-bus read --agent claude`
- Synthesizes cross-agent findings
- Makes commit/fix decisions

### Specialist Agents (subagents)

Each agent has a defined role, target files, and bus reporting requirements.

| Agent Type | Role | Bus Topic | stm32-merge Targets |
|------------|------|-----------|---------------------|
| `architect-reviewer` | Module coupling, build coherence, dependency graph | `arch-findings` | CMakeLists.txt, Cargo.toml, module_registry |
| `security-auditor` | Credentials, supply chain, memory safety, IEC 62304 | `security-findings` | USB drivers, vendor/, .env, configs |
| `performance-engineer` | Memory layout, ISR timing, build optimization | `perf-findings` | Linker scripts, audio engine, FreeRTOS config |
| `c-pro` | C/C++ code quality, embedded patterns | `cpp-findings` | src/**/*.{c,h,cpp} |
| `rust-pro` | Rust companion quality, Cargo optimization | `rust-findings` | stm32-remote/**/*.rs |
| `test-automator` | Test coverage gaps, missing assertions | `test-findings` | tests/**, build_unit_tests/ |
| `debugger` | Build errors, runtime failures, crash analysis | `debug-findings` | Build logs, crash dumps |

### Existing stm32-merge Agents (leverage, don't duplicate)

These are already defined in the repo and should be invoked by name rather than recreated:
- `.codex/agents/build-troubleshooter.md` — firmware build issues
- `.codex/agents/hardfault-analyzer.md` — Cortex-M7 crash analysis
- `.codex/agents/usb-validator.md` — USB descriptor/enumeration validation
- `dhf-tracker/agents/traceability-analyzer.md` — IEC 62304 requirement tracing
- `dhf-tracker/agents/semantic-gap-analyzer.md` — requirement coverage gaps
- `dhf-tracker/commands/dhf-validate.md` — full DHF traceability validation

## Bus Reporting Protocol

### Message Format

Every agent MUST post findings to the bus using this structure:

```bash
agent-bus send \
  --from-agent <agent-type> \
  --to-agent claude \
  --topic "<topic>-findings" \
  --body "<structured finding, max 2000 chars>" \
  --tag "repo:stm32-merge" \
  --tag "severity:high|medium|low" \
  --priority normal \
  --encoding compact
```

### Finding Body Structure (max 2000 chars)

```
FINDING: <one-line summary>
SEVERITY: HIGH|MEDIUM|LOW
FILE: <path:line>
CURRENT: <what exists now>
PROPOSED: <what should change>
RATIONALE: <why, max 200 chars>
STATUS: discovered|proposed|fixed|verified
```

Agents update STATUS by posting follow-up messages with the same `--thread-id`.

### Reporting Frequency

- **On discovery**: Post immediately when a finding is identified
- **On completion**: Post a summary message with total finding count
- **On fix**: Post a status update with `STATUS: fixed` and the commit SHA

### Bus Read Patterns

Orchestrator uses:
```bash
# All findings from current session
agent-bus read --agent claude --since-minutes 60 --encoding human

# Findings by severity
agent-bus read --agent claude --from-agent security --since-minutes 60

# Watch for real-time updates during long runs
agent-bus watch --agent claude --history 20 --encoding human
```

## Orchestration Workflow

### Phase 1: Infrastructure Check
```
1. agent-bus health --encoding json  (verify Redis + PG)
2. Announce session start to bus
```

### Phase 2: Parallel Analysis (read-only)
```
3. Dispatch 3-5 specialist agents in parallel
4. Each agent:
   a. Reads target files in the repo
   b. Uses available MCP tools (ast-grep, context7, etc.)
   c. Uses repo-specific tools (dhf-validate, build-troubleshooter)
   d. Posts findings to bus (one message per finding, max 2000 chars)
   e. Posts completion summary
5. Orchestrator reads bus, synthesizes cross-agent themes
```

### Phase 3: Fix Implementation (write, sequential)
```
6. Orchestrator prioritizes findings by severity
7. For each P0/P1 fix:
   a. Dispatch a specialist agent (rust-pro, c-pro, etc.) with the fix spec
   b. Agent implements fix, runs tests
   c. Agent posts STATUS: fixed to bus with commit info
   d. Orchestrator verifies via bus read
8. Commit batch with conventional commit messages
```

### Phase 4: Verification
```
9. Dispatch test-automator to run full test suite
10. Dispatch code-reviewer for post-fix review
11. Both report results to bus
12. Orchestrator synthesizes final status
```

## Agent Prompt Template

Each dispatched agent receives this preamble:

```
You are a {agent-type} analyzing {repo-path}.
Report ALL findings to the agent bus using:
  agent-bus send --from-agent {agent-id} --to-agent claude \
    --topic "{topic}-findings" --body "<FINDING>" \
    --tag "repo:{repo}" --tag "severity:{severity}" --encoding compact

Finding format (max 2000 chars):
FINDING: <summary>
SEVERITY: HIGH|MEDIUM|LOW
FILE: <path:line>
CURRENT: <what exists>
PROPOSED: <what should change>
RATIONALE: <why>
STATUS: discovered

Available repo tools: {list of repo-specific commands/agents}
Available MCP tools: ast-grep, context7, desktop-commander
```

## Benchmarking Data Collection

Each orchestration run produces metrics posted to the bus:

```bash
agent-bus send --from-agent claude --to-agent all --topic "benchmark" \
  --body "SESSION: agents=3, duration=12min, tokens=338K, findings=18, fixes=4, bus_msgs=24" \
  --tag "type:benchmark" --encoding compact
```

Over time this builds a dataset in PostgreSQL for analyzing:
- Token cost per finding
- Agent effectiveness by type
- Fix success rate
- Bus message overhead vs finding value

## Config-Driven Agent Selection

The orchestrator selects agents based on the repo's technology stack:

| Repo Signal | Agents Dispatched |
|-------------|-------------------|
| `*.rs` present | rust-pro |
| `*.c` / `*.h` present | c-pro, performance-engineer |
| `CMakeLists.txt` | architect-reviewer |
| `dhf-tracker/` | traceability-analyzer (repo agent) |
| `.github/workflows/` | deployment-engineer |
| `Cargo.toml` | security-auditor (cargo-audit) |
| `vcpkg.json` | security-auditor (supply chain) |
