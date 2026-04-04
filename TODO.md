# Agent Bus - Active Roadmap

Structural execution plan:
- See [`agents.TODO.md`](./agents.TODO.md) for the canonical completion plan for the remaining package/crate refactor work.
- Code-grounded status snapshot: [`docs/current-status-2026-04-03.md`](./docs/current-status-2026-04-03.md).
- Phase 3 crate split plan: [`docs/phase3-crate-split-plan-2026-04-04.md`](./docs/phase3-crate-split-plan-2026-04-04.md).

## Current Baseline

- Rust-only runtime: deprecated Python package, pytest suite, and PyO3 codec crate removed from the repo.
- Cargo workspace exists and `agent-bus-core` now owns shared storage, validation, token, and typed ops logic, but the runtime split is still incomplete.
- Current transport surface: CLI (35 subcommands), HTTP (30 routes), MCP stdio + Streamable HTTP (17 tools), channels, ownership claims, task queue, session summary, dedup, dashboard, TOON/minimal encodings.
- Validation now includes local deploy health checks plus an SSE notification smoke test.
- Durable notification streams now back direct HTTP replay and the MCP `check_inbox` cursor path.
- Direct-recipient Pub/Sub now bridges back into `/events/{agent}` so live SSE listeners can be nudged by messages posted from other processes, not only by the HTTP handler that accepted the request.
- Structural split Phase 1 (ops consolidation) and Phase 2 (transport normalization) complete: `validated_post_message`, `validated_batch_send`, `apply_service_action`, input validation on claim/presence ops, and HTTP compact-context consolidated to shared op. Core ops layer now ~1,670 lines.
- Agent profile abstraction added (`AgentProfile` trait with `Codex`, `Claude`, `Gemini`, `Custom` variants) replacing Codex-specific bridge logic.
- Validated task cards added (`TaskCard`, `TaskStatus`, `push_task_card`, `pull_task_card`, `peek_task_cards`) alongside existing opaque string API for backward compat.
- 486 tests total (394 unit in agent-bus-core, 92 in rust-cli); `http_integration_test.rs` is a skeleton with 0 test functions.
- Phase 3 crate split plan documented in `docs/phase3-crate-split-plan-2026-04-04.md`.

## P0 Direct Signaling

- [x] Add durable per-agent notification streams alongside the canonical message stream.
- [x] Add a first-class `knock` / attention signal over the durable direct-notification path.
- [ ] Add explicit subscription records for recipient, repo, session, thread, tag, topic, priority, and resource scopes.
- [ ] Promote `thread_id` to a joinable conversation scope with explicit membership.
- [ ] Add ack deadlines, reminder delivery, and escalation for `request_ack=true` messages.
- [x] Add notification replay for reconnecting SSE and MCP clients.

## P1 Query Model And Cross-Repo Awareness

- [x] Add first-class `repo`, `session`, `tag`, and `thread_id` filters to CLI, HTTP, and MCP read paths.
- [ ] Stop over-fetching for `session-summary`, `dedup`, and related commands; route tagged queries through PostgreSQL indexes when available.
- [ ] Add repo/session-scoped inbox cursors instead of one global cursor per agent.
- [ ] Add repo/session inventory commands: active repos, active sessions, agents by repo, open claims by repo.
- [ ] Add first-class thread summaries/compaction for `thread_id` and direct channels; current `session-summary` is useful only when `session:<id>` tags are present, but live coordination often centers on `thread_id` (`wezterm-joint-plan-*`, resource threads) instead.

## P2 Cross-Agent Orchestration

- [x] Replace opaque task queue strings with validated task cards: `repo`, `paths`, `priority`, `depends_on`, `reply_to`, `tags`, `status`. Core types added (`TaskCard`, `TaskStatus`, `push_task_card`, `pull_task_card`, `peek_task_cards`); transport wiring pending.
- [x] Add first-class lease-backed claims with `shared`, `shared_namespaced`, and `exclusive` modes so `RESOURCE_START` / `RESOURCE_DONE` stops living only in free-text messages.
- [ ] Add durable resource-event notifications and subscriptions over `resource_id`, repo, path prefix, scope kind, and `thread_id`.
- [ ] Add server-assisted reroute suggestions for namespaced resources (`cargo target`, coverage, bench output, temp install validation) so agents can isolate instead of wait.
- [x] Add TTL-based resource renewal/expiry for lease-backed claims.
- [ ] Add ack deadlines for high-risk resources such as `~/bin` installs, user config, services, and repo-default artifact roots.
- [ ] Add cross-repo resource scopes for machine-global paths and services so one repo view can still see contention caused by another repo.
- [ ] Surface claims, task queues, pending ACKs, and contested ownership directly in the dashboard.
- [ ] Add server-side orchestrator summaries for "what changed since last poll" instead of forcing agents to replay raw history.
- [x] Generalize `codex_bridge.rs` into agent profiles/adapters so sync workflows are not Codex-specific. `AgentProfile` trait added with `Codex`, `Claude`, `Gemini`, `Custom` variants; `codex_bridge.rs` refactored to use it.

## P3 Token Efficiency

- [ ] Add `summarize-session` and `summarize-inbox` APIs that emit compact rollups for LLM consumers.
- [ ] Extend compact/minimal encodings to shorten repeated tag prefixes and schema markers.
- [ ] Add excerpt mode for long findings so reads can return short bodies plus metadata pointers.
- [ ] Benchmark token and byte reduction across JSON, compact, minimal, TOON, and MessagePack for realistic multi-agent sessions.
- [ ] Add a `compact-thread` / `summarize-thread` flow for direct-channel work so agents can resume a single coordination thread without replaying unrelated repo traffic.
- [x] Add a machine-safe mode for machine-readable encodings (`toon`, `compact`, `minimal`, JSON) that suppresses non-fatal warnings from stdout/stderr mixing; current PostgreSQL fallback warnings pollute shell-captured LLM context.

## P4 Validation And Testing

- [x] Add deploy-time SSE notification smoke validation after local deploy.
- [x] Add HTTP integration coverage for direct-notification replay via `/notifications/{agent}`.
- [x] Add reusable PowerShell smoke harnesses for CLI, HTTP, and forced degraded PostgreSQL mode so CI can separate Redis failures from PostgreSQL fallback behavior.
- [ ] Add runtime tests for CLI server-mode via `AGENT_BUS_SERVER_URL` now that the async transport client path is in place.
- [ ] Add end-to-end MCP tool tests for `check_inbox`, channels, claims, and negotiation.
- [ ] Add HTTP integration coverage for `/dashboard`, `/tasks/:agent`, and `/token-count`.
- [x] Add HTTP integration coverage for `/compact-context`.
- [x] Add HTTP/operator coverage for maintenance controls so pause/resume gating is exercised locally before deploys.
- [ ] Add mixed multi-repo session tests that prove repo/session scoping prevents inbox bleed-through.
- [ ] Add regression coverage for PostgreSQL-backed reads/compaction where optional metadata fields are serialized through `jsonb`; the direct read bug was fixed, but remaining query paths still need regression coverage so future `jsonb` bind mismatches do not silently fall back to Redis.
- [ ] Add CLI/HTTP parity tests for `read-direct`, `compact-context`, and future thread-summary flows using real direct-channel traffic, not only main-stream messages.

## P5 Docs And Operations

- [x] Add a short query-model section to `README.md` explaining Redis-first reads, PostgreSQL tag queries, and current client-side filtering limits.
- [x] Expand `AGENT_COMMUNICATIONS.md` with a recommended multi-repo tagging contract: `repo:<name>`, `session:<id>`, `wave:<n>`, `task:<id>`.
- [ ] Document `qmd` usage and troubleshooting; current indexing exists, but `query/search` timeouts need operator guidance.
- [ ] Package reproducible bootstrap artifacts for new machines without relying on local path assumptions.
- [x] Add built-in maintenance/service lifecycle controls for `agent-bus-http.exe` and document how deploy/update flows should pause, flush, stop, and restart the service.
- [x] Document when to prefer `agent-bus-http.exe` over `agent-bus.exe`: frequent send/read loops against the long-running HTTP service, token-sensitive reads (`toon`, `compact-context`), and direct/group channel work vs. backend debugging or MCP server startup.
- [x] Add positive and negative operator examples for `agent-bus-http.exe` in `README.md` and `AGENT_COMMUNICATIONS.md`, including narrow repo/thread filters, direct-channel reads, and anti-patterns such as broad unfiltered reads during multi-repo sessions.
- [x] Capture one short real-world coordination example from `repo:wezterm` traffic in the public docs to show the recommended `repo:<name>` + `thread_id` + `RESOURCE_START` / `RESOURCE_DONE` pattern without depending on internal-only paths.

## P6 Build, Toolchain, And Repo Simplification

- [x] Centralize Rust build environment setup across `build.ps1`, `validate-agent-bus.ps1`, `build-deploy.ps1`, `bootstrap.ps1`, and `setup-agent-hub-local.ps1` so target-dir policy, `sccache`, and linker selection stop drifting.
- [x] Add a fast local iteration profile (`fast-release`) for compile validation and local binary refreshes that do not need full release link cost.
- [x] Prefer `cargo nextest` for local validation when available, with `cargo test` fallback and serialized integration runs for the shared Redis/PostgreSQL suite.
- [x] Check in a repo-scoped `.cargo/config.toml` with cargo aliases for the active `rust-cli/` crate plus stable Windows linker defaults so repo-local `cargo` use matches the wrapper scripts more closely.
- [x] Introduce a library-backed crate root with a thin `main.rs` wrapper so future CLI/HTTP/MCP surface splits can reuse one runtime entry implementation.
- [x] Extend the checked-in cargo config to automate `sccache` defaults once that behavior is validated across clean Windows machines and non-operator shells.
- [x] Add dedicated `agent-bus-http` and `agent-bus-mcp` bin targets on top of the shared library runtime so the deploy surface no longer depends on copying one CLI executable under multiple names.
- [ ] Finish the current partial workspace split into `agent-bus-core`, `agent-bus-cli`, `agent-bus-http`, and `agent-bus-mcp` so the surface crates stop depending on `rust-cli` and the deploy/build scripts stop targeting `rust-cli` as the authoritative crate.
- [x] Replace the CLI `reqwest::blocking` + thread-spawn server-mode bridge with a shared async transport client so `AGENT_BUS_SERVER_URL` no longer needs separate blocking behavior.
- [x] Extract the first shared `ops` module for send/ack/knock/presence flows so CLI, HTTP, and MCP stop maintaining separate write-path behavior for those verbs.
- [x] Extract a typed operations/service layer so CLI, HTTP, and MCP stop duplicating dispatch, validation glue, and JSON shaping.
- [x] Remove tracked binary artifacts and empty/stale top-level directories (`bin/agent-bus.exe`, top-level `rust/`, top-level `src/`, top-level `tests/`) before the public release.
- [x] Record the structural refactor sequence in `docs/structural-refactor-plan-2026-03-25.md` so the repo has an explicit path from today’s library-backed package to a future multi-bin and multi-crate layout.
- [ ] Execute the remaining structural steps captured in [`agents.TODO.md`](./agents.TODO.md) until the workspace split is operationally complete and the shared service layer leaves CLI, HTTP, and MCP as thin surfaces.
