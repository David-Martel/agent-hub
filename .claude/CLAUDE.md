# .claude/CLAUDE.md — Local Best Practices

Supplements the root [`CLAUDE.md`](../CLAUDE.md). This file covers gotchas,
workflow guardrails, and cross-references that agents and contributors hit
in practice.

## Key Reference Files

| File | Purpose |
|------|---------|
| [`CLAUDE.md`](../CLAUDE.md) | Architecture, build commands, module table, env vars |
| [`AGENTS.md`](../AGENTS.md) | Coding style, testing, commit conventions, CI gates |
| [`AGENT_COMMUNICATIONS.md`](../AGENT_COMMUNICATIONS.md) | Full multi-agent protocol: 5-step quick start, schemas, binary selection, examples |
| [`TODO.md`](../TODO.md) | Active roadmap (P0-P6), current baseline, open items |
| [`agents.TODO.md`](../agents.TODO.md) | Canonical structural refactor plan (crate split, service layer) |
| [`docs/structural-refactor-plan-2026-03-25.md`](../docs/structural-refactor-plan-2026-03-25.md) | Path from single crate to multi-bin/multi-crate layout |
| [`docs/phase3-crate-split-plan-2026-04-04.md`](../docs/phase3-crate-split-plan-2026-04-04.md) | Phase 3 dependency analysis and migration sequence |
| [`docs/direct-signaling-proposal-2026-03-23.md`](../docs/direct-signaling-proposal-2026-03-23.md) | Durable notification and knock design |
| [`docs/resource-coordination-proposal-2026-03-24.md`](../docs/resource-coordination-proposal-2026-03-24.md) | Lease-backed claims and resource scoping |

## Cargo Aliases (repo-scoped `.cargo/config.toml`)

Run these from the repo root — no need to `cd rust-cli`:

```bash
cargo ab-build        # cargo build for rust-cli
cargo ab-fast         # fast-release profile (thin LTO off, incremental on)
cargo ab-test         # unit tests only (--bin agent-bus)
cargo ab-itest        # integration tests (serial, needs Redis + PG)
cargo ab-clippy       # clippy for rust-cli
cargo ab-nextest      # nextest runner (if installed)
```

## Gotchas and Common Mistakes

### Redis port is 6380, not 6379
The default `AGENT_BUS_REDIS_URL` is `redis://localhost:6380/0`. This is
intentional — the bus uses a dedicated Redis instance, not the system default.

### Integration tests require live services
`cargo ab-itest` needs Redis on `:6380` and PostgreSQL on `:5300`. Unit tests
(`cargo ab-test`) have no external dependencies. Always run unit tests first.

### `server-mode` feature gate
The `reqwest` dependency and all CLI server-mode routing are behind
`features = ["server-mode"]` (on by default). If you add a new server-mode
code path, gate it with `#[cfg(feature = "server-mode")]`.

### sccache wrapper
The checked-in `.cargo/config.toml` sets `rustc-wrapper = "scripts/rustc-wrapper.cmd"`
which delegates to sccache when available. To bypass for debugging:
```bash
RUSTC_WRAPPER="" cargo build
```

### Lefthook hooks run on multiple stages
Lefthook runs on pre-commit, pre-push, and commit-msg (not just pre-push):
- **pre-commit**: `cargo fmt --check`, `cargo clippy`, `ast-grep scan` (parallel)
- **pre-push**: `cargo test`, `cargo audit` (parallel)
- **commit-msg**: conventional commit format advisory

Install with `lefthook install` if hooks are missing.

### `main.rs` is a stub
All runtime logic lives in `lib.rs`. The three binaries (`main.rs`,
`bin/agent-bus-http.rs`, `bin/agent-bus-mcp.rs`) are thin entry points
calling `main_entry()`, `http_entry()`, or `mcp_entry()` respectively.
New commands go in `cli.rs` (Clap) + `commands.rs` (implementation).

### Non-obvious modules in `rust-cli/src/`
`codex_bridge.rs` handles Codex-specific sync workflows (planned for
generalization into agent profiles — see TODO.md P2). `mcp_discovery.rs`
handles MCP capability negotiation and tool schema advertisement.

### Task queue commands are new
`push-task`, `pull-task`, `peek-tasks` are recent additions. They use
simple Redis lists (`bus:tasks:<agent>`). The TODO.md tracks replacing
these with validated task cards (P2).

### MCP tool parity with CLI
Not every CLI command has an MCP tool equivalent. The MCP surface (17 tools)
covers the core coordination verbs. CLI-only commands like `journal`, `monitor`,
`codex-sync`, `service`, `session-summary`, `dedup`, `token-count`, and
`compact-context` are operator/scripting tools not exposed via MCP.

### PostgreSQL is optional but important
Redis is always required. PostgreSQL enables durable history, tag-indexed queries,
and the `check_inbox` cursor path. Without PG, the bus falls back to Redis-only
with client-side filtering. The circuit breaker (60s cooldown) handles PG outages
gracefully — but watch for fallback warnings polluting stdout in machine-readable
encodings (fixed for `toon`/`compact`/`minimal`/`json`).

## Workflow Best Practices

### Before modifying any module
1. Read [`AGENTS.md`](../AGENTS.md) for style and testing expectations.
2. Check [`TODO.md`](../TODO.md) for whether the area has open roadmap items.
3. Check [`agents.TODO.md`](../agents.TODO.md) if touching module boundaries or crate structure.
4. Use `pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease` for fast iteration builds from repo root.

### Before adding a new CLI subcommand
1. Add the `Cmd` variant in `cli.rs` with Clap derive attributes.
2. Add the match arm in `lib.rs::run()`.
3. Implement in `commands.rs`, calling shared ops from `ops.rs` where possible.
4. If the command should also work via HTTP, add the route in `http.rs`.
5. Update the CLI subcommands list in the root `CLAUDE.md`.

### Before adding a new MCP tool
1. Add the schema function in `mcp.rs::schemas`.
2. Add the `Tool::new(...)` entry in `tool_list()`.
3. Add the dispatch match arm in `call_tool()`.
4. Add a test asserting the tool name exists in `tool_list_names_are_correct`.
5. Update the MCP tool count and list in the root `CLAUDE.md`.

### Before modifying the protocol
[`AGENT_COMMUNICATIONS.md`](../AGENT_COMMUNICATIONS.md) is the source of truth
for the agent-facing protocol. If you change message schemas, topic conventions,
or the 5-step quick start, update that file — not just code comments.

### Running smoke tests after deploys
```powershell
pwsh -NoLogo -NoProfile -File scripts/test-agent-bus-cli-smoke.ps1
pwsh -NoLogo -NoProfile -File scripts/test-agent-bus-http-smoke.ps1
pwsh -NoLogo -NoProfile -File scripts/test-agent-bus-sse-smoke.ps1
pwsh -NoLogo -NoProfile -File scripts/test-agent-bus-functional.ps1
```

## Encoding Selection Guide

| Encoding | Use case | Token cost |
|----------|----------|------------|
| `json` | Machine parsing, external integrations | Highest |
| `compact` | Structured but smaller JSON | Medium |
| `human` | Operator dashboards, debugging | Medium |
| `toon` | LLM context windows, agent reads | ~30% of JSON |
| `msgpack` | Internal wire format (LZ4-compressed bodies) | N/A (binary) |

Default to `--encoding toon` for agent-to-agent reads. Use `json` only when
downstream tooling requires it. MessagePack (`rmp-serde`) is used internally
for compressed message bodies, not as a user-facing encoding flag.

## Multi-Agent Coordination Checklist

Per [`AGENT_COMMUNICATIONS.md`](../AGENT_COMMUNICATIONS.md):

1. **Announce** — `set_presence` or `send` with `topic=status` on session start.
2. **Claim** — `claim` before editing any file; check for conflicts.
3. **Inbox** — `check_inbox` (MCP) or `read` (CLI) every 2-3 tool calls.
4. **Schema** — Use `--schema finding` for findings, `--schema status` for status.
5. **Tags** — Always include `repo:<name>`. Add `session:<id>`, `thread_id` for scoping.
6. **Complete** — Post a COMPLETE summary when done, then poll for follow-up.
