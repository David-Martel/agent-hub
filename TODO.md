# Agent Bus — Active Roadmap

## Current baseline

- Rust-only runtime: deprecated Python package, pytest suite, and PyO3 codec crate removed from the repo.
- Validated locally on 2026-03-22:
  - `cargo test --bin agent-bus`
  - `cargo test --test integration_test --test channel_integration_test -- --test-threads=1`
  - `cargo test --test http_integration_test -- --test-threads=1`
- Current transport surface: CLI, HTTP, MCP stdio, MCP Streamable HTTP, channels, ownership claims, task queue, session summary, dedup, dashboard, TOON/minimal encodings.

## P1 Query model and cross-repo awareness

- [ ] Add first-class `repo`, `session`, `tag`, and `thread_id` filters to CLI, HTTP, and MCP read paths.
- [ ] Stop over-fetching for `session-summary`, `dedup`, and related commands; route tagged queries through PostgreSQL indexes when available.
- [ ] Add repo/session-scoped inbox cursors instead of one global cursor per agent.
- [ ] Add repo/session inventory commands: active repos, active sessions, agents by repo, open claims by repo.

## P2 Cross-agent orchestration

- [ ] Replace opaque task queue strings with validated task cards: `repo`, `paths`, `priority`, `depends_on`, `reply_to`, `tags`, `status`.
- [ ] Surface claims, task queues, pending ACKs, and contested ownership directly in the dashboard.
- [ ] Add server-side orchestrator summaries for “what changed since last poll” instead of forcing agents to replay raw history.
- [ ] Generalize `codex_bridge.rs` into agent profiles/adapters so sync workflows are not Codex-specific.

## P3 Token efficiency

- [ ] Add `summarize-session` and `summarize-inbox` APIs that emit compact rollups for LLM consumers.
- [ ] Extend compact/minimal encodings to shorten repeated tag prefixes and schema markers.
- [ ] Add excerpt mode for long findings so reads can return short bodies plus metadata pointers.
- [ ] Benchmark token and byte reduction across JSON, compact, minimal, TOON, and MessagePack for realistic multi-agent sessions.

## P4 Validation and testing

- [ ] Add runtime tests for CLI server-mode via `AGENT_BUS_SERVER_URL`.
- [ ] Add end-to-end MCP tool tests for `check_inbox`, channels, claims, and negotiation.
- [ ] Add HTTP integration coverage for `/dashboard`, `/tasks/:agent`, `/token-count`, and `/compact-context`.
- [ ] Add mixed multi-repo session tests that prove repo/session scoping prevents inbox bleed-through.

## P5 Docs and operations

- [ ] Add a short query-model section to `README.md` explaining Redis-first reads, PostgreSQL tag queries, and current client-side filtering limits.
- [ ] Expand `AGENT_COMMUNICATIONS.md` with a recommended multi-repo tagging contract: `repo:<name>`, `session:<id>`, `wave:<n>`, `task:<id>`.
- [ ] Document `qmd` usage and troubleshooting; current indexing exists, but `query/search` timeouts need operator guidance.
- [ ] Package reproducible bootstrap artifacts for new machines without relying on local path assumptions.
