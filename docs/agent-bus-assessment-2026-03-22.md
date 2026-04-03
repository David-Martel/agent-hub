# Agent Bus Assessment (2026-03-22)

> Historical assessment. For the current code-grounded status, see [`docs/current-status-2026-04-03.md`](./current-status-2026-04-03.md).

## What Is Already Validated

- `cargo test --bin agent-bus` passed locally: 375 Rust unit tests.
- The codebase has working coverage for core Redis/PostgreSQL flows, schema validation, CLI parsing, token minimization, ownership state, and dashboard HTML generation.
- Integration suites exist for CLI/channel flows and HTTP flows, but coverage is uneven across the newer surfaces.

## What Still Needs Completion, Testing, Or Validation

### Runtime validation gaps

- Server-mode CLI (`AGENT_BUS_SERVER_URL`) is implemented in `commands.rs`, but there is no matching integration coverage for the CLI-over-HTTP path.
- MCP coverage is mostly schema/dispatch validation; the repo does not yet exercise live MCP tool calls end to end for channels, claims, or `check_inbox`.
- The dashboard, task queue endpoints, `token-count`, and `compact-context` exist, but they are not covered in the current integration tests.
- Multi-repo behavior is protocol-driven rather than first-class in the query APIs: most read paths filter only by recipient/sender, not by `repo:*`, `session:*`, or `thread_id`.

### Documentation gaps

- `AGENT_COMMUNICATIONS.md` was stale versus the implementation: it still described 8 MCP tools even though `mcp.rs` exposes 14.
- MCP bootstrap instructions still told agents to poll with `list_messages`; this is less efficient than the cursor-based `check_inbox` flow.
- `TODO.md` is historical, but its closing sections still read like an active roadmap. A dedicated forward-looking roadmap doc would be clearer.
- `qmd` indexing is present in this workspace, but query/search calls timed out during this review. That integration needs an operator note with troubleshooting and timeout expectations.

## Highest-Value Product Improvements

### More useful

- Add tag-scoped reads everywhere: CLI, HTTP, and MCP should support `tag`, `repo`, `session`, and `thread_id` filters.
- Promote task payloads from opaque strings to validated task cards with `repo`, `paths`, `priority`, `depends_on`, and `reply_to`.
- Generalize `codex_bridge.rs` into agent profiles/adapters so cross-agent sync is not Codex-specific.

### More token-efficient

- Add a server-side `summarize-session` / `summarize-inbox` API that emits compact rollups instead of raw message replay.
- Extend compact output to shorten repeated tag prefixes (`repo:`, `session:`, `severity:`) and emit optional schema codes.
- Support body excerpts + metadata pointers for long finding batches rather than repeating full bodies in every read path.

### More cross-repo and cross-agent aware

- Make inbox cursors scope-aware: `(agent, repo)` or `(agent, session)` instead of one global cursor per agent.
- Add query surfaces for “active repos”, “active sessions”, “agents by repo”, and “open ownership claims by repo”.
- Surface claims, pending ACKs, and task queues in the dashboard so orchestrators can see coordination state in one place.

## Recommended Documentation Updates

1. Add a `docs/roadmap.md` that replaces the stale “all TODO items complete” ending with current backlog categories: validation, protocol evolution, packaging, and observability.
2. Add a concise “Query Model” section to `README.md` explaining Redis-first reads, PostgreSQL tag queries, and where over-fetch/client-side filtering still exists.
3. Add a “Multi-Repo Session Pattern” section to `AGENT_COMMUNICATIONS.md` with recommended tags: `repo:<name>`, `session:<id>`, `wave:<n>`, `task:<id>`.
