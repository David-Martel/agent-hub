# Agent Bus

Rust-native coordination bus for Codex, Claude, Gemini, and local sub-agents.

The active implementation lives in `rust-cli/` and uses Redis for live
transport plus PostgreSQL for durable history and presence-event persistence.
Deprecated Python runtime code has been removed from this repository.

## Code-Grounded Status (2026-04-03)

- The repository now has a top-level Cargo workspace with `rust-cli`,
  `agent-bus-core`, `agent-bus-cli`, `agent-bus-http`, and `agent-bus-mcp`.
- `agent-bus-core` already owns most shared storage, validation, token, and
  typed ops logic.
- The structural split is still in progress: `rust-cli` remains the primary
  runtime crate and still owns the large CLI, HTTP, and MCP transport modules.
- The `agent-bus-cli`, `agent-bus-http`, and `agent-bus-mcp` crates currently
  act as thin wrapper crates that call into `rust-cli`.
- The build, validate, and deploy scripts still treat `rust-cli` as the
  authoritative build root.

See [docs/current-status-2026-04-03.md](./docs/current-status-2026-04-03.md)
for the full code-grounded status snapshot and remaining required work.

Current protocol metadata:
- Bus protocol: `agent-bus` message contract v1.0
- Message identity: UUID `id` + UTC `timestamp_utc`
- Default Redis endpoint: `redis://localhost:6380/0`
- Default PostgreSQL endpoint: `postgresql://postgres@localhost:5300/redis_backend`
- Redis install fallback: [redis-windows/redis-windows](https://github.com/redis-windows/redis-windows)

## Commands

```powershell
agent-bus health --encoding compact
agent-bus send --from-agent codex --to-agent claude --topic status --body "message"
agent-bus knock --from-agent codex --to-agent claude --body "check thread wezterm-joint-plan-20260324"
agent-bus claim src/redis_bus.rs --agent claude --mode shared_namespaced --namespace claude-target --lease-ttl-seconds 900
agent-bus renew-claim src/redis_bus.rs --agent claude --lease-ttl-seconds 900
agent-bus release-claim src/redis_bus.rs --agent claude
agent-bus read --agent codex --since-minutes 120
agent-bus service --action status
agent-bus service --action pause --reason "deploy maintenance"
agent-bus service --action restart --reason "rotate agent-bus-http"
pwsh -NoLogo -NoProfile -File build.ps1 -Release
pwsh -NoLogo -NoProfile -File scripts\validate-agent-bus.ps1
pwsh -NoLogo -NoProfile -File scripts\build-deploy.ps1
pwsh -NoLogo -NoProfile -File scripts\install-mcp-clients.ps1
pwsh -NoLogo -NoProfile -File scripts\test-agent-bus-functional.ps1
```

## Binary Roles

- `agent-bus.exe`: CLI + MCP stdio entry point. Use it for `serve --transport stdio`, backend health checks, admin commands, and local debugging.
- `agent-bus-http.exe`: long-running HTTP/SSE service variant. Use it for frequent `send` / `read` loops, `/notifications/{agent}` replay, SSE subscribers, and Windows service installs.
- `agent-bus-mcp.exe`: MCP-focused wrapper binary. With no arguments it defaults to `serve --transport stdio`, but today it still delegates into the shared `rust-cli` runtime rather than an independent MCP-only crate implementation.

## Local Build Orchestration

- `build.ps1` is the top-level local build/test entry point for this repo.
- It imports `CargoTools` when available, then applies the repo's shared Rust build helper so `CARGO_TARGET_DIR`, `sccache`, `lld-link`, and local iteration flags are configured consistently across build, validate, deploy, and bootstrap flows.
- By default it writes to a private `CARGO_TARGET_DIR` namespace under the active machine target root so concurrent repo work does not fight over the same cargo lock. Use `-TargetNamespace <name>` or `-TargetDir <path>` to make that namespace explicit.
- Local validation prefers `cargo nextest` when available and falls back to `cargo test` otherwise. Integration targets still run serially against the shared Redis/PostgreSQL services.
- A repo-scoped [`.cargo/config.toml`](./.cargo/config.toml) now adds `cargo ab-fast`, `cargo ab-build`, `cargo ab-test`, `cargo ab-itest`, `cargo ab-clippy`, and `cargo ab-nextest` aliases so direct cargo use from the repo root still targets `rust-cli/` consistently.
- The checked-in cargo config now also installs a repo-local `rustc-wrapper` shim, so plain shell `cargo` runs automatically use `sccache` when it is present without requiring operator profile setup.
- The repo is now a workspace, but operationally it still behaves like a partially split runtime: `agent-bus-core` owns extracted shared modules while `rust-cli` still owns runtime startup plus the large `commands.rs`, `http.rs`, and `mcp.rs` surfaces.
- The wrapper crates under `crates/agent-bus-cli`, `crates/agent-bus-http`, and `crates/agent-bus-mcp` currently call back into `rust-cli`; they do not yet remove `rust-cli` from the build graph.
- Shared typed ops now live in [`crates/agent-bus-core/src/ops/mod.rs`](./crates/agent-bus-core/src/ops/mod.rs), while [`rust-cli/src/ops/mod.rs`](./rust-cli/src/ops/mod.rs) acts as a compatibility shim.
- The structural split plan and current checkpoint are documented in [docs/structural-refactor-plan-2026-03-25.md](./docs/structural-refactor-plan-2026-03-25.md) and [docs/current-status-2026-04-03.md](./docs/current-status-2026-04-03.md).
- Use `pwsh -NoLogo -NoProfile -File build.ps1 -Release` for a full optimized build, or `pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease` for a faster local iteration binary that avoids the full release link cost.
- The shared helper now restarts a stale `sccache` server automatically when the resident server version does not match the installed client, which avoids noisy post-build stats failures after tool upgrades.
- CLI server-mode reads and admin calls now use the async `reqwest` client path internally rather than a separate blocking client plus worker thread bridge.
- Use `cargo ab-clippy --target-dir <dir> -- -D warnings` when you want strict clippy with an explicit target dir; the alias now leaves room for extra cargo args instead of hard-coding the lint terminator.

## Dependency Modes

- Redis is required for functional messaging. If Redis is down, CLI and HTTP smoke should fail.
- PostgreSQL is optional but recommended. When PostgreSQL is unavailable, the bus should stay live with `database_ok=false`, Redis-backed reads, and degraded history/query depth.
- On fresh Windows machines, use [redis-windows/redis-windows](https://github.com/redis-windows/redis-windows) as the supported Redis install path.

## Query Model

- `read`, `export`, `journal`, `session-summary`, and similar history-oriented queries prefer PostgreSQL when it is configured, then fall back to Redis if the PostgreSQL query path fails.
- `compact-context` is intentionally Redis-first. It now supports `agent`, `repo`, `session`, `tag`, and `thread_id` scoping so token compaction stays aligned with the live coordination stream instead of replaying unrelated repo traffic.
- Use `repo:<name>` tags on every message. Add `session:<id>` for bounded work waves and `thread_id` for long-running shared plans or resource handoffs.
- Set `AGENT_BUS_MACHINE_SAFE=true` when piping machine-readable output into another tool or LLM capture and you want to suppress non-fatal degraded-mode fallback warnings.
- Claim routes and server-mode claim commands now percent-encode resource paths correctly, so file-like resources such as `src/http.rs` survive the HTTP path layer.

## Leases And Knock

- Lease-backed claims now support `--mode shared|shared_namespaced|exclusive`, `--namespace`, `--scope-kind`, `--scope-path`, `--repo-scope`, `--thread-id`, and `--lease-ttl-seconds`.
- Use `renew-claim` for long-running work and `release-claim` when the resource is free again. Expired claims are pruned automatically.
- Use `knock` for urgent direct attention when a peer should check the bus immediately. Knocks are durable direct notifications, not transient terminal-only pings.
- The HTTP service now bridges Redis Pub/Sub back into `/events/{agent}` delivery, so direct messages and knocks posted by other processes can still wake active SSE listeners.

## Maintenance And Service Control

- `agent-bus service --action status` reports both the Windows service state and the live HTTP admin maintenance state when the server is reachable.
- `agent-bus service --action pause --reason "deploy maintenance"` puts the HTTP service into maintenance mode so mutating routes return `503` while reads, health, dashboard, SSE, and notification replay keep working.
- `agent-bus service --action flush` forces the in-process PostgreSQL writer to drain before you archive data or rotate the binary.
- `agent-bus service --action stop` requests pause + flush, then stops the Windows `AgentHub` service when it is installed. If no Windows service is present, it falls back to a graceful in-process stop for a standalone HTTP server.
- `agent-bus service --action start` and `agent-bus service --action restart` manage the installed Windows service directly and then wait for `/health` to come back.
- HTTP operators can use `GET /admin/service` and `POST /admin/service/control` directly. Supported control actions are `pause`, `resume`, `flush`, and `stop`.

## Operator Examples

- Good: `agent-bus-http.exe compact-context --agent claude --repo wezterm --tag planning --thread-id wezterm-joint-plan-20260324 --since-minutes 120 --max-tokens 2000`
- Good: `agent-bus-http.exe read-direct --agent-a codex --agent-b claude --limit 20 --encoding toon`
- Good: `agent-bus-http.exe knock --from-agent codex --to-agent claude --body "claim review ready" --request-ack`
- Good: `agent-bus claim T:\RustCache\cargo-target --agent codex --mode shared_namespaced --namespace codex-build --scope-kind artifact_root --repo-scope agent-bus`
- Good: `agent-bus health --encoding compact`
- Avoid: `agent-bus-http.exe read --agent claude --since-minutes 1440 --encoding toon` with no `repo`, `session`, `tag`, or `thread_id` during multi-repo work.
- Avoid: treating `watch` as the durable source of record. Use it as a notification probe, then follow up with scoped `read`, `read-direct`, or `compact-context`.

## WezTerm Pattern

Recent Codex/Claude work in the `wezterm` repo converged on a stable pattern that is worth copying:

- Tag every coordination message with `repo:wezterm`.
- Use a shared `thread_id` such as `wezterm-joint-plan-20260324` for planning continuity.
- Use explicit `RESOURCE_START` and `RESOURCE_DONE` messages for shared paths or deployment/PR ownership.
- Back those messages with a lease-backed claim when the resource is long-lived or exclusive.
- Prefer `read-direct` for pairwise handoffs and scoped `compact-context` for “what changed in this one thread?” context recovery.
- Health-check `agent-bus-http.exe` before long waves instead of assuming the service is still up.

### Windows service

```powershell
# Install the service (requires admin + nssm in PATH)
pwsh -NoLogo -NoProfile -File scripts\install-agent-hub-service.ps1

# Configure log rotation (10 MB max, daily rotation)
pwsh -NoLogo -NoProfile -File scripts\configure-log-rotation.ps1

# Remove the service
pwsh -NoLogo -NoProfile -File scripts\remove-agent-hub-service.ps1
```

#### Service troubleshooting

```powershell
# Check service status
agent-bus service --action status

# Restart the service
agent-bus service --action restart --reason "manual operator restart"

# Pause writes before maintenance, then resume later
agent-bus service --action pause --reason "archive + deploy"
agent-bus service --action resume --reason "maintenance complete"

# View recent logs
Get-Content C:\ProgramData\AgentHub\logs\agent-hub-service.log -Tail 50

# View error logs
Get-Content C:\ProgramData\AgentHub\logs\agent-hub-service-error.log -Tail 50
```

Common issues:
- **Port in use**: Another process is using port 8400. Check with `netstat -ano | findstr 8400`.
- **Redis not running**: Verify with `redis-cli -p 6380 ping`. If you need a local Redis install, use the maintained [redis-windows](https://github.com/redis-windows/redis-windows) repository.
- **PostgreSQL connection refused**: Verify with `psql -h localhost -p 5300 -U postgres -c "SELECT 1"`.
- **Service won't start**: Check error log, ensure `%USERPROFILE%\bin\agent-bus-http.exe` exists.

### MCP launch

```powershell
agent-bus serve --transport stdio
agent-bus serve --transport mcp-http --port 8765
```

## Notes

- Live message durability and fanout use Redis Streams + Pub/Sub.
- Direct-recipient attention routing now derives durable per-agent notification streams from the canonical message log.
- Durable history and presence events are mirrored into PostgreSQL when configured.
- Real joint Codex/Claude sessions have converged on narrow scoping: keep `repo:<name>` tags on every message, set `thread_id` for shared planning, and use explicit `RESOURCE_START` / `RESOURCE_DONE` handoffs for shared paths.
- For token-sensitive recovery, prefer scoped `compact-context` over broad inbox replay.
- Realtime watch uses Redis Pub/Sub.
- `GET /notifications/{agent}` replays durable notification envelopes for reconnecting HTTP clients.
- `POST /knock` sends a high-priority durable direct attention signal; active `/events/{agent}` listeners now receive it even when another process posted the source message.
- `AgentHub` should run from `%USERPROFILE%\bin\agent-bus-http.exe` so stdio MCP sessions on `agent-bus.exe` or `agent-bus-mcp.exe` do not block service deploys.
- PowerShell wrapper scripts call the Rust binary directly and default to local `localhost` Redis/PostgreSQL services.
- `scripts\build-deploy.ps1` now uses the shared Rust build helper, prefers `sccache`/`lld-link` when present, deploys dedicated CLI/HTTP/MCP binaries, requests pause/flush through the built-in service controls before stopping the service, then verifies the HTTP health endpoint and runs a live SSE notification smoke test after restart.
- `scripts\validate-agent-bus.ps1` now uses the same helper and builds with the `fast-release` profile by default before running tests, health, and optional smoke checks.
- `scripts\install-mcp-clients.ps1` updates Claude and Codex MCP configs in-place, with timestamped backups by default.
- `scripts/install-agent-hub-service.ps1` installs the native HTTP transport as a Windows service using `nssm` + `pwsh` and sets `AGENT_BUS_SERVICE_NAME` for the runtime.
- MCP clients should register the stdio form by default.
- Streamable HTTP is available for clients that support long-lived MCP sessions and notifications.
- MCP server validates Redis and PostgreSQL during startup and emits a startup status message from `agent-bus`.
- Redis remains the realtime system of record; PostgreSQL is the query/history backend.
- Sample client configs are in `examples/mcp/`.
- A tiny browser client lives under `web/` for manual HTTP probing; it is intentionally static and not auto-served by the Rust binary yet.
- Agent coordination guidance is in `AGENT_COMMUNICATIONS.md` and `MCP_CONFIGURATION.md`.
- Current architecture and backlog status are summarized in [docs/current-status-2026-04-03.md](./docs/current-status-2026-04-03.md).

## Encoding and interoperability

- Default CLI output remains human-readable JSON; prefer `--encoding compact` for LLM prompts and scripts.
- `thread_id` can be used for grouped coordination conversations.
- `protocol_version` is added to all outbound payloads for safe schema evolution.
- Planned future: adapter mode for A2A task cards and protobuf/TOON payload negotiation for selected endpoints.

### Interop & runtime metadata

- `bus_health` now returns `protocol_version` and `runtime_metadata` for monitoring hooks.
- `bus_health.runtime_metadata.codec` reports the active Rust serializer backend and is safe for observability pipelines.
- Suggested integration order:
  - Use compact JSON for all machine pipelines and script-to-script transport.
  - Use human mode only for direct operator viewing.
  - Keep payload contracts additive (`protocol_version`, `thread_id`) to preserve forward compatibility.
- See [IMPLEMENTATION_NOTES.md](./IMPLEMENTATION_NOTES.md) for a concrete A2A/TOON/protobuf comparison and Rust migration sequence.

### Performance roadmap

- High-throughput work now focuses on Redis/PostgreSQL query efficiency, server-side summaries, and better repo/session filtering rather than Python fallback paths.
- See [IMPLEMENTATION_NOTES.md](./IMPLEMENTATION_NOTES.md) and [docs/agent-bus-assessment-2026-03-22.md](./docs/agent-bus-assessment-2026-03-22.md) for the current interop and roadmap notes.

### Validation And Deploy

- `pwsh -NoLogo -NoProfile -File scripts\validate-agent-bus.ps1` builds with `fast-release`, runs tests, checks health, and runs the CLI + HTTP functional smoke harness.
- `pwsh -NoLogo -NoProfile -File build.ps1 -Release -TargetNamespace codex-local` runs the repo-root build/test flow in a private cargo target namespace and can hand off to the same local smoke harness.
- `pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease -SkipSmoke -TargetNamespace codex-iter` is the fastest local compile-validation loop when you want a near-release binary without waiting for a full optimized link.
- `pwsh -NoLogo -NoProfile -File scripts\test-agent-bus-cli-smoke.ps1` validates CLI health, presence, send, and read against a live Redis-backed bus.
- `pwsh -NoLogo -NoProfile -File scripts\test-agent-bus-http-smoke.ps1` starts `agent-bus-http.exe`, checks `/health`, `/messages`, `/notifications/{agent}`, and the live SSE path.
- `pwsh -NoLogo -NoProfile -File scripts\test-agent-bus-functional.ps1` runs both smoke layers and forces a degraded PostgreSQL mode to verify Redis-only fallback behavior.
- `pwsh -NoLogo -NoProfile -File scripts\build-deploy.ps1 -TargetDir T:\RustCache\cargo-target\codex-http-deploy` rebuilds from an isolated release target dir, refreshes `%USERPROFILE%\bin\agent-bus.exe`, `%USERPROFILE%\bin\agent-bus-http.exe`, and `%USERPROFILE%\bin\agent-bus-mcp.exe`, then restarts the long-running HTTP service.
- `pwsh -NoLogo -NoProfile -File scripts\build-deploy.ps1` rebuilds the release binaries, refreshes the installed binary paths, restarts the service, prints a health summary, and runs the SSE smoke test.
- `pwsh -NoLogo -NoProfile -File scripts\setup-agent-hub-local.ps1` installs the local binaries and validates CLI health against Redis/PostgreSQL.
- `pwsh -NoLogo -NoProfile -File scripts\install-mcp-clients.ps1` installs the stdio MCP entry for Claude and Codex.
- Use `-SkipSmoke` on either script when you only want build/restart validation.
- These validations are intended to run on the local/self-hosted Windows machine; they do not depend on GitHub-hosted action minutes.

### CI And Release Coverage

- CI now separates unit tests, integration tests, CLI/HTTP smoke, and benchmarks so failures are attributable to the CLI surface, the HTTP/SSE surface, or infra availability.
- The workflow is designed for the existing self-hosted runner. The authoritative validation path remains the local PowerShell harnesses in `scripts\`.
- The smoke harness records whether PostgreSQL was healthy or degraded, rather than hiding that distinction behind a generic green test run.
- Bench CI runs both `throughput` and `bus_benchmarks`, then uploads Criterion output when available so Redis-backed hot-path regressions are visible over time.
