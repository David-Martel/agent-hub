# Agent Bus - Active Roadmap

Structural execution plan:
- See [`agents.TODO.md`](./agents.TODO.md) for remaining surface-thinning work.
- Code-grounded status snapshot: [`docs/current-status-2026-06-13.md`](./docs/current-status-2026-06-13.md).
- Phase 3 crate split plan: [`docs/phase3-crate-split-plan-2026-04-04.md`](./docs/phase3-crate-split-plan-2026-04-04.md) is complete.

## Current Baseline

- Rust-only runtime: deprecated Python package, pytest suite, and PyO3 codec crate removed from the repo.
- Workspace split landed: `rust-cli` is gone. The workspace is now `agent-bus-core` plus the `agent-bus-cli`, `agent-bus-http`, and `agent-bus-mcp` surface crates (verified `cargo check --workspace` clean, 2026-06-13). `agent-bus-cli` is the `agent-bus` binary package (v0.5.0) and still pulls `axum`/`rmcp`/`reqwest` because `serve` starts HTTP/MCP inline — it is not yet a thin surface.
- Current transport surface: CLI (35 subcommands), HTTP (30 routes), MCP stdio + Streamable HTTP (17 tools), channels, ownership claims, task queue, session summary, dedup, dashboard, TOON/minimal encodings.
- Validation now includes local deploy health checks plus an SSE notification smoke test.
- Durable notification streams now back direct HTTP replay and the MCP `check_inbox` cursor path.
- Direct-recipient Pub/Sub now bridges back into `/events/{agent}` so live SSE listeners can be nudged by messages posted from other processes, not only by the HTTP handler that accepted the request.
- Structural split Phase 1 (ops consolidation) and Phase 2 (transport normalization) complete: `validated_post_message`, `validated_batch_send`, `apply_service_action`, input validation on claim/presence ops, and HTTP compact-context consolidated to shared op. Core ops layer now ~1,670 lines.
- Agent profile abstraction added (`AgentProfile` trait with `Codex`, `Claude`, `Gemini`, `Custom` variants) replacing Codex-specific bridge logic.
- Validated task cards added (`TaskCard`, `TaskStatus`, `push_task_card`, `pull_task_card`, `peek_task_cards`) alongside existing opaque string API for backward compat.
- Integration tests now live under `crates/agent-bus-cli/tests/`: `http_integration_test.rs` (47 test fns, no longer a skeleton), `channel_integration_test.rs` (6), `integration_test.rs` (5), plus the large unit suite in `agent-bus-core`.
- Phase 3 crate split plan documented in `docs/phase3-crate-split-plan-2026-04-04.md` and marked complete.
- Post-split CI, release, build, examples, installer, and MCP client remediation merged in PR #11 (2026-06-14).

## P0 Direct Signaling

- [x] Add durable per-agent notification streams alongside the canonical message stream.
- [x] Add a first-class `knock` / attention signal over the durable direct-notification path.
- [x] Add explicit subscription records for recipient, repo, session, thread, tag, topic, priority, and resource scopes. `Subscription` + `SubscriptionScopes` models; Redis storage with optional TTL; CLI subscribe/unsubscribe/subscriptions commands; HTTP CRUD endpoints; `message_matches_subscription` filter.
- [x] Promote `thread_id` to a joinable conversation scope with explicit membership. `Thread` model + `ThreadStatus` + Redis storage + CLI thread-create/join/leave/list/close + HTTP CRUD.
- [x] Add ack deadlines, reminder delivery, and escalation for `request_ack=true` messages. `AckDeadline` model with priority-based timeouts and escalation levels; HTTP `GET /overdue-acks`.
- [x] Add notification replay for reconnecting SSE and MCP clients.

## P1 Query Model And Cross-Repo Awareness

- [x] Add first-class `repo`, `session`, `tag`, and `thread_id` filters to CLI, HTTP, and MCP read paths.
- [x] Stop over-fetching for `session-summary`, `dedup`, and related commands; route tagged queries through PostgreSQL indexes when available. `query_messages_by_tags()` added; `bus_list_messages_with_filters()` now prefers PG GIN path when tags/thread present.
- [x] Add repo/session-scoped inbox cursors instead of one global cursor per agent. `check_inbox` now uses `bus:notify_cursor:<agent>:repo:<repo>` or `:session:<session>` keys when filters are set; backward compatible.
- [x] Add repo/session inventory commands: active repos, active sessions, agents by repo, open claims by repo. CLI `inventory` + HTTP `GET /inventory` with `?repo=` drill-down.
- [x] Add first-class thread summaries/compaction for `thread_id` and direct channels. `summarize_thread()` + `compact_thread()` in core ops; CLI `summarize-thread` + `compact-thread` commands; HTTP `GET /thread-summary` + `POST /compact-thread` endpoints.

## P2 Cross-Agent Orchestration

- [x] Replace opaque task queue strings with validated task cards: `repo`, `paths`, `priority`, `depends_on`, `reply_to`, `tags`, `status`. Core types added (`TaskCard`, `TaskStatus`, `push_task_card`, `pull_task_card`, `peek_task_cards`); transport wiring pending.
- [x] Add first-class lease-backed claims with `shared`, `shared_namespaced`, and `exclusive` modes so `RESOURCE_START` / `RESOURCE_DONE` stops living only in free-text messages.
- [x] Add durable resource-event notifications and subscriptions over `resource_id`, repo, path prefix, scope kind, and `thread_id`. `ResourceEvent` model + Redis streams (`agent_bus:resource_events:<id>`, MAXLEN 1000); events emitted on claim/renew/release/resolve; HTTP `GET /resource-events/<id>`.
- [x] Add server-assisted reroute suggestions for namespaced resources (`cargo target`, coverage, bench output, temp install validation) so agents can isolate instead of wait. `RerouteSuggestion` struct + pattern matching for 7 known resource types; enriches contested claim responses.
- [x] Add TTL-based resource renewal/expiry for lease-backed claims.
- [x] Add ack deadlines for high-risk resources such as `~/bin` installs, user config, services, and repo-default artifact roots. Shared infrastructure with P0 ack deadlines above.
- [x] Add cross-repo resource scopes for machine-global paths and services so one repo view can still see contention caused by another repo. `ResourceScope` enum (Repo/Machine) + auto-detection for 9 known machine-global resources; CLI `--scope` flag; HTTP/MCP scope field.
- [x] Surface claims, task queues, pending ACKs, and contested ownership directly in the dashboard. `/dashboard/data` now returns health, presence, claims (total + contested resources), pending ACKs, and per-agent task queue depths.
- [x] Add server-side orchestrator summaries for "what changed since last poll" instead of forcing agents to replay raw history. `OrchestratorSummary` with categorized notification counts; HTTP `GET /orchestrator-summary`.
- [x] Generalize `codex_bridge.rs` into agent profiles/adapters so sync workflows are not Codex-specific. `AgentProfile` trait added with `Codex`, `Claude`, `Gemini`, `Custom` variants; `codex_bridge.rs` refactored to use it.

## P3 Token Efficiency

- [x] Add `summarize-session` and `summarize-inbox` APIs that emit compact rollups for LLM consumers. `summarize_session()` + `summarize_inbox()` + `aggregate_messages()` in core ops; CLI refactored to delegate; HTTP `GET /session-summary`.
- [x] Extend compact/minimal encodings to shorten repeated tag prefixes and schema markers. `shorten_tags()` applied in compact/minimal modes: `repo:X→r:X`, `session:X→s:X`, `thread_id:X→t:X`, `severity:X→!X`.
- [x] Add excerpt mode for long findings so reads can return short bodies plus metadata pointers. `excerpt_body()` + CLI `--excerpt N` + HTTP `?excerpt=N`.
- [x] Benchmark token and byte reduction across JSON, compact, minimal, TOON, and MessagePack for realistic multi-agent sessions. Criterion benchmark with 50-message corpus; TOON at 32.9% of JSON, tag shortening at 33.8% byte reduction.
- [x] Add a `compact-thread` / `summarize-thread` flow for direct-channel work so agents can resume a single coordination thread without replaying unrelated repo traffic. `compact_thread()` + `summarize_thread()` in core ops; CLI + HTTP endpoints.
- [x] Add a machine-safe mode for machine-readable encodings (`toon`, `compact`, `minimal`, JSON) that suppresses non-fatal warnings from stdout/stderr mixing; current PostgreSQL fallback warnings pollute shell-captured LLM context.

## P4 Validation And Testing

- [x] Add deploy-time SSE notification smoke validation after local deploy.
- [x] Add HTTP integration coverage for direct-notification replay via `/notifications/{agent}`.
- [x] Add reusable PowerShell smoke harnesses for CLI, HTTP, and forced degraded PostgreSQL mode so CI can separate Redis failures from PostgreSQL fallback behavior.
- [x] Add runtime tests for CLI server-mode via `AGENT_BUS_SERVER_URL` now that the async transport client path is in place.
- [x] Add end-to-end MCP tool tests for `check_inbox`, channels, claims, and negotiation. `crates/agent-bus-mcp/tests/mcp_tools_e2e.rs` (7 tests) drives `McpToolDispatch` directly — the shared path both stdio and `mcp-http` transports use. Ran live: 7 passed.
- [x] Add HTTP integration coverage for `/dashboard`, `/tasks/:agent`, and `/token-count`.
- [x] Add HTTP integration coverage for `/compact-context`.
- [x] Add HTTP/operator coverage for maintenance controls so pause/resume gating is exercised locally before deploys.
- [x] Add mixed multi-repo session tests that prove repo/session scoping prevents inbox bleed-through.
- [x] Add regression coverage for PostgreSQL-backed reads/compaction where optional metadata fields are serialized through `jsonb`. `crates/agent-bus-core/tests/postgres_jsonb_roundtrip.rs` (2 tests) round-trips rich/empty metadata through `persist_message_postgres` → `list_messages_postgres_with_filters`. Ran live: 2 passed.
- [x] Add CLI/HTTP parity tests for `read-direct`, `compact-context`, and thread-summary flows using real direct-channel traffic. `crates/agent-bus-cli/tests/cli_http_parity_test.rs` (3 tests, ran live: 3 passed).

## P5 Docs And Operations

- [x] Add a short query-model section to `README.md` explaining Redis-first reads, PostgreSQL tag queries, and current client-side filtering limits.
- [x] Expand `AGENT_COMMUNICATIONS.md` with a recommended multi-repo tagging contract: `repo:<name>`, `session:<id>`, `wave:<n>`, `task:<id>`.
- [x] Document `qmd` usage and troubleshooting; `query/search` timeout operator guidance. See `docs/qmd-operator-guide.md`.
- [ ] Package reproducible bootstrap artifacts for new machines without relying on local path assumptions.
- [x] Add built-in maintenance/service lifecycle controls for `agent-bus-http.exe` and document how deploy/update flows should pause, flush, stop, and restart the service.
- [x] Document when to prefer `agent-bus-http.exe` over `agent-bus.exe`: frequent send/read loops against the long-running HTTP service, token-sensitive reads (`toon`, `compact-context`), and direct/group channel work vs. backend debugging or MCP server startup.
- [x] Add positive and negative operator examples for `agent-bus-http.exe` in `README.md` and `AGENT_COMMUNICATIONS.md`, including narrow repo/thread filters, direct-channel reads, and anti-patterns such as broad unfiltered reads during multi-repo sessions.
- [x] Capture one short real-world coordination example from `repo:wezterm` traffic in the public docs to show the recommended `repo:<name>` + `thread_id` + `RESOURCE_START` / `RESOURCE_DONE` pattern without depending on internal-only paths.

## P6 Build, Toolchain, And Repo Simplification

- [x] Centralize Rust build environment setup across `build.ps1`, `validate-agent-bus.ps1`, `build-deploy.ps1`, `bootstrap.ps1`, and `setup-agent-hub-local.ps1` so target-dir policy, `sccache`, and linker selection stop drifting.
- [x] Add a fast local iteration profile (`fast-release`) for compile validation and local binary refreshes that do not need full release link cost.
- [x] Prefer `cargo nextest` for local validation when available, with `cargo test` fallback and serialized integration runs for the shared Redis/PostgreSQL suite.
- [x] Check in a repo-scoped `.cargo/config.toml` with cargo aliases for the active `agent-bus` package plus stable Windows linker defaults so repo-local `cargo` use matches the wrapper scripts more closely.
- [x] Introduce a library-backed crate root with a thin `main.rs` wrapper so future CLI/HTTP/MCP surface splits can reuse one runtime entry implementation.
- [x] Extend the checked-in cargo config to automate `sccache` defaults once that behavior is validated across clean Windows machines and non-operator shells.
- [x] Add dedicated `agent-bus-http` and `agent-bus-mcp` bin targets on top of the shared library runtime so the deploy surface no longer depends on copying one CLI executable under multiple names.
- [x] Finish the current partial workspace split into `agent-bus-core`, `agent-bus-cli`, `agent-bus-http`, and `agent-bus-mcp` so the surface crates stop depending on `rust-cli` and the deploy/build scripts stop targeting `rust-cli` as the authoritative crate.
- [x] Replace the CLI `reqwest::blocking` + thread-spawn server-mode bridge with a shared async transport client so `AGENT_BUS_SERVER_URL` no longer needs separate blocking behavior.
- [x] Extract the first shared `ops` module for send/ack/knock/presence flows so CLI, HTTP, and MCP stop maintaining separate write-path behavior for those verbs.
- [x] Extract a typed operations/service layer so CLI, HTTP, and MCP stop duplicating dispatch, validation glue, and JSON shaping.
- [x] Remove tracked binary artifacts and empty/stale top-level directories (`bin/agent-bus.exe`, top-level `rust/`, top-level `src/`, top-level `tests/`) before the public release.
- [x] Record the structural refactor sequence in `docs/structural-refactor-plan-2026-03-25.md` so the repo has an explicit path from today’s library-backed package to a future multi-bin and multi-crate layout.
- [x] Execute the remaining structural steps captured in [`agents.TODO.md`](./agents.TODO.md) until the workspace split is operationally complete and the shared service layer leaves CLI, HTTP, and MCP as thin surfaces.

## P7 Storage Optimization (from POSTGRES-REDIS.md)

- [x] Switch message IDs from UUIDv4 to UUIDv7 for timestamp-ordered B-tree inserts. 5 call sites changed; subscription/task card IDs remain v4.
- [x] Add `MAXLEN ~` stream trimming to all XADD calls. Already present on all streams (100000 main, 10000 notifications, 1000 resource events/channels).
- [x] Add TTL/EXPIRE to Redis keys by purpose: notifications (3d), cursors (7d), tasks (3d), resource events (3d), direct channels (7d), groups (30d).
- [x] Clean up 193+ stale test keys matching `bus:direct:orchestrator:resolve-*` and `bus:direct:test-*`. (Requires live Redis connection.)

## P8 Post-Split Remediation (closed 2026-06-14)

The `rust-cli` crate was removed and its contents redistributed into the four
workspace crates. Remediation merged in PR #11; the remaining structural item
is tracked in `agents.TODO.md` as CLI thin-surface cleanup.

- [x] **CRITICAL** — Re-anchor `.github/workflows/ci.yml` onto the workspace root (`cargo fmt/clippy/test --workspace`; integration via `-p agent-bus`). Added a gated `cross-platform` Docker job.
- [x] **CRITICAL** — Re-anchor `.github/workflows/release.yml`; version-match step now reads `crates/agent-bus-cli/Cargo.toml`.
- [x] **CRITICAL** — Fix `build.ps1`: dropped `$rustCliDir`/the throw; builds from the workspace manifest. Verified runs to "Build orchestration complete." (exit 0).
- [x] Fix `sgconfig.yml` ast-grep globs (`rust-cli/src/**` → `crates/*/src/**`).
- [x] Commit the in-flight migration (scripts, examples, MCP_CONFIGURATION.md/README.md, moved tests).
- [x] Repo hygiene: removed committed scratch (`errors.json`, `build.log`) + gitignored scratch globs. (Run the `docs/public-release-checklist.md` secret/path grep before a public tag.)
- [x] Refresh stale architecture docs: new `docs/current-status-2026-06-13.md` (+ superseded banner on the 04-03 doc), `phase3-crate-split-plan` COMPLETE banner, `README.md` crate map, `CLAUDE.md` rewritten, `agents.TODO.md` checkpoint/test inventory refreshed.
- [x] Reformat the whole tree (`cargo fmt --all`) — drift had accumulated because CI's fmt check pointed at the removed `rust-cli/`.
- [ ] Make `agent-bus-cli` a genuinely thin surface: resolve the Phase 3 "CLI serve" decision (shell out to `agent-bus-http`/`-mcp`, depend on those crates as libraries, or drop `serve` from the CLI) so the CLI binary stops linking `axum`/`rmcp` just to host the inline server. Remove the leftover re-export shim files in `crates/agent-bus-cli/src/` (`channels.rs`, `redis_bus.rs`, `postgres_store.rs`, `output.rs`, `settings.rs`, `token.rs`, `validation.rs`, `models.rs`, `journal.rs`, `codex_bridge.rs`) once callers import from `agent-bus-core` directly. **(STILL OPEN)**

## P9 Robustness Follow-Ups (from PR #7/#8 audit)

- [x] Wire automatic PG backfill on reconnect (deferred F1 from PR #7): `spawn_pg_backfill_monitor` in `crates/agent-bus-http/src/http.rs` runs at the HTTP bootstrap layer where both handles are available; reads the Redis stream and calls `postgres_store::backfill_dropped_writes` inside `spawn_blocking`, gated on circuit-closed + `pg_dropped_writes > 0`. Storage layers stay decoupled (no Redis handle in `postgres_store.rs`).
- [x] Add a regression test for the dropped-writes path. `dropped_writes_increments_while_breaker_open_then_backfill_clears_gap` in `postgres_store.rs` tests. Ran live: passed.
- [x] Audit message sinks for centralized NUL/length validation. Added shared `reject_nul_in_fields` (`validation.rs`) and routed task-card/presence/subscription/resource-event sinks through it; per-sink NUL-rejection unit tests added.

## P10 Cross-Platform & Acceleration (NEW — landed 2026-06-13)

- [x] Cross-platform validation via system Docker: `docker/Dockerfile.linux` (glibc + optional musl), `.dockerignore`, `scripts/cross-validate.{ps1,sh}` (local-first, no-op-with-message if Docker absent). Linux build verified: workspace compiles + 590 lib tests pass.
- [x] Rust acceleration tooling: `[profile.profiling]` added; `.cargo/config.toml` aliases fixed (`-p agent-bus-cli` → `-p agent-bus`) + new `ab-cov`/`ab-bench`/`ab-audit`/`ab-machete`/`ab-flame`; `docs/rust-acceleration.md` operator guide; `scripts/install-rust-tooling.{ps1,sh}` idempotent installers (nextest, llvm-cov, flamegraph, samply, machete, audit, tokio-console).
- [x] Add a native Ubuntu CI job for workspace format and lib/bin unit tests.
- [ ] Wire the `cross-platform` Docker and Ubuntu jobs into the required-checks set once they have run green a few times.
- [ ] Optional: add a musl static-binary release artifact to `release.yml` for Linux distribution (Dockerfile already supports the musl target).

## P11 Security, Installers, Networking, And Offline Ops (NEW — 2026-06-14)

- [x] Apply Dependabot security updates for `rmcp`, `openssl`, and `rustls-webpki` to close GitHub alerts GHSA-89vp-x53w-74fx, GHSA-82j2-j2ch-gfr8, GHSA-965h-392x-2mh5, GHSA-xgp8-3hg3-c2mh, and related OpenSSL alerts.
- [x] Add `.github/dependabot.yml` so Cargo and GitHub Actions updates are scheduled and grouped explicitly.
- [x] Add `--dry-run` / `-DryRun` previews for high-risk installers, build/deploy validation, Docker cross-validation, MCP client config installation, service removal/log rotation, and bus write smoke helpers.
- [x] Extend Redis pub/sub consumers to use deterministic loopback URL candidates so `localhost` works with IPv4-only or IPv6-only loopback backends.
- [x] Make HTTP bind address formatting IPv6-literal safe and add an offline `/support` page with health, loopback, recovery, and remote-auth guidance.
- [ ] Make `cargo audit` blocking once CI has proven the fresh `cargo-audit` install handles current RustSec CVSS data reliably. Workstation note: `cargo-audit 0.21.2` cannot parse CVSS 4.0 RustSec advisories; `cargo-audit 0.22.2` install also exposed global `sccache` transport failures and a failed final binary move, so keep audit best-effort until the tool install path is reliable.
- [ ] Add token rotation support: multiple accepted bearer tokens or token-file input while preserving `AGENT_BUS_AUTH_TOKEN`.
- [ ] Split unauthenticated liveness from authenticated readiness/full health without breaking existing `/health` clients.
- [ ] Add native Linux install/service docs and scripts for systemd-managed `agent-bus-http`.
- [ ] Add offline installation profile: preinstalled Rust/Cargo cache, Redis/PostgreSQL prerequisites, and no-internet runtime support statement.
