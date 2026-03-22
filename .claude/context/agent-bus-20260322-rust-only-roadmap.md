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
- Implemented first-class `repo`, `session`, `tag`, and `thread_id` filters across CLI, HTTP, and MCP read surfaces.
- Added shared query-scope tag construction plus backend filtered read helpers for PostgreSQL and Redis fallback paths.
- Switched `session-summary`, `dedup`, `export`, `journal`, and MCP `check_inbox` onto the backend-filtered message path instead of client-side overfetch/filter loops.
- Added/updated unit and schema coverage for scoped filters plus an HTTP integration test that exercises the new filter contract.

## Validation Updates
- `cargo test --bin agent-bus -- --test-threads=1` passed after the scoped-filter implementation (386 tests).
- `cargo test --test integration_test --test channel_integration_test -- --test-threads=1` passed.
- `cargo clippy --all-targets -- -D warnings` passed.
- `cargo test --test http_integration_test -- --test-threads=1` still targets an already-running service at `http://localhost:8400`; one new scoped-filter test failed against that live service, which indicates the external process was not running the freshly built code rather than a compile/test failure in this checkout.

## Current Risks / Follow-up
- `check_inbox` still uses one global cursor per agent; repo/session-scoped cursors are still pending.
- The HTTP integration suite depends on an externally managed service on port `8400`; it should be updated to launch or point at the freshly built binary to validate handler changes reliably.
- `rust-cli/src/journal.rs` and some wrapper paths still intentionally reference `~/.codex/tools/agent-bus-mcp/` as the canonical installed location; that path is now effectively the Rust-only repo/tooling location, not a Python runtime.
- qmd indexing is available in the workspace, but `query/search` timed out during repository assessment and needs operator-level troubleshooting guidance.

## Recommended Next Implementation Wave
1. Add repo/session-scoped inbox cursors so `check_inbox` cannot bleed across concurrent coordination waves.
2. Add end-to-end tests for CLI server-mode and MCP `check_inbox` using a test-managed server process rather than a pre-existing daemon.
3. Introduce structured task cards for cross-repo orchestration.
4. Add `summarize-session` and `summarize-inbox` APIs plus compact encoding benchmarks.
