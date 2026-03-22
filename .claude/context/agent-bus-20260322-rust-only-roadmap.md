# Agent Bus Rust-Only Roadmap Context (2026-03-22)

## Project State
- **Branch:** main
- **Runtime:** Rust-only; deprecated Python runtime code removed from repo
- **Validation:** `cargo test --bin agent-bus` passed after cleanup (375 tests)
- **Context snapshot:** `ctx-agent-bus-20260322-103327` saved under `~/.codex/context/projects/agent-bus/`

## What Changed
- Enforced deprecation by removing the tracked Python package, pytest suite, native-codec crate, and Python build script.
- Rewrote `DEPRECATION.md` to reflect removal rather than partial deprecation.
- Replaced the historical `TODO.md` with an active roadmap focused on:
  - repo/session-aware queries
  - structured task cards
  - token-efficient summaries
  - server-mode and MCP end-to-end validation
  - qmd/documentation hardening
- Updated `README.md`, `IMPLEMENTATION_NOTES.md`, `AGENTS.md`, `rust-cli/src/cli.rs`, and `rust-cli/src/main.rs` to remove stale Python guidance.
- Updated MCP protocol instructions to prefer `check_inbox` over replaying `list_messages`.

## Current Risks / Follow-up
- `rust-cli/src/journal.rs` and some wrapper paths still intentionally reference `~/.codex/tools/agent-bus-mcp/` as the canonical installed location; that path is now effectively the Rust-only repo/tooling location, not a Python runtime.
- Remaining roadmap work should start with first-class repo/session/tag/thread filters and scoped inbox cursors.
- qmd indexing is available in the workspace, but `query/search` timed out during repository assessment and needs operator-level troubleshooting guidance.

## Recommended Next Implementation Wave
1. Add query filters for `repo`, `session`, `tag`, and `thread_id` across CLI, HTTP, and MCP.
2. Route session/dedup flows through indexed Postgres tag queries instead of over-fetching.
3. Add end-to-end tests for CLI server-mode and MCP `check_inbox`.
4. Introduce structured task cards for cross-repo orchestration.
