# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Redis + PostgreSQL agent coordination bus for multi-agent systems (Claude,
Codex, Gemini). The workspace contains exactly four crates; `rust-cli` has
been removed.

Current crate layout:

- `crates/agent-bus-core` — shared library: storage adapters (redis_bus,
  postgres_store), sync_store (Redis half of cloud sync), channels, settings, models, token helpers, validation,
  output, journal, codex_bridge, agent_profile, bootstrap, mcp_dispatch
  (shared `McpToolDispatch`), error, and ops subtree.
- `crates/agent-bus-cli` — package name `agent-bus`, produces the `agent-bus`
  binary. Owns cli.rs, commands.rs, lib.rs, server_mode.rs, monitor.rs,
  mcp_discovery.rs. Still links axum/rmcp/reqwest because `serve` starts
  HTTP/MCP inline. Depends on `agent-bus-core`.
- `crates/agent-bus-http` — package name `agent-bus-http`, produces the
  `agent-bus-http` binary. Owns http.rs and cloud_sync.rs (the opt-in hub to
  cloud sync task, see `docs/cloud-sync.md`). Depends only on `agent-bus-core`
  plus Axum/rmcp/Redis/reqwest.
- `crates/agent-bus-mcp` — package name `agent-bus-mcp`, produces the
  `agent-bus-mcp` binary. Owns mcp.rs. Lightest dependency footprint.

**Note on agent-bus-cli:** `lib.rs` still declares private shim modules
(`channels`, `redis_bus`, `postgres_store`, etc.) that re-export from
`agent-bus-core`. These are internal to the CLI crate and will be cleaned up
in a follow-on pass. The canonical implementations live in `agent-bus-core`.

Current binary surfaces:

- `agent-bus.exe` (from `crates/agent-bus-cli`)
- `agent-bus-http.exe` (from `crates/agent-bus-http`)
- `agent-bus-mcp.exe` (from `crates/agent-bus-mcp`)

Storage is dual: Redis (realtime streams + pub/sub) with PostgreSQL (durable
history, tag-indexed queries).

**Config**: Settings load from `~/.config/agent-bus/config.json` → env vars → hardcoded defaults.

## Build & Development

Structural refactor tracking (split complete; remaining cleanup):
- [`agents.TODO.md`](./agents.TODO.md)

Code-grounded status snapshot (post-split):
- [`docs/current-status-2026-06-13.md`](./docs/current-status-2026-06-13.md)

```bash
# Preferred repo-root local build/test entrypoint
pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease

# Workspace sanity check (unit tests, no external services needed)
cargo test --workspace --lib --bins

# Build all binaries, release profile
cargo build --workspace --release --bins

# Clippy (required — pre-push hook enforces)
cargo clippy --workspace --all-targets -- -D warnings

# Tests (unit only, no external services)
cargo test --workspace --lib --bins

# Backend/integration tests are #[ignore]d (#64): they need DISPOSABLE backends via
# AGENT_BUS_TEST_REDIS_URL / _DATABASE_URL / _SERVER_URL, refuse the live ports
# 6380/5300/8400, and FAIL (never skip) when unset. Without --ignored they run 0 tests.
cargo test --workspace --tests -- --ignored --test-threads=1   # == cargo ab-itest

# Benchmarks (criterion: token estimation, TOON, MessagePack)
cargo bench --workspace

# Format check
cargo fmt --all --check

# Build + Deploy + Restart service (one command)
pwsh -NoLogo -NoProfile -File scripts/build-deploy.ps1
# Or skip build: scripts/build-deploy.ps1 -SkipBuild
```

**sccache note**: Repo-local cargo usage now goes through the checked-in
`.cargo/config.toml` wrapper and uses `sccache` automatically when present. If
you need to bypass that path for debugging, override `RUSTC_WRAPPER=""` in the
current shell.

## Architecture

Workspace layout (`rust-cli` has been removed):

- `crates/agent-bus-core`
  - shared library depended on by all three binary crates
  - storage adapters: `redis_bus`, `postgres_store`
  - channels, settings, models, token helpers, validation, output, journal
  - `codex_bridge.rs` — Codex-specific sync helpers
  - `agent_profile.rs` — `AgentProfile` trait
  - `bootstrap.rs` — `bootstrap()` + `BootstrapGuard` (PgWriter init, tracing, startup announce)
  - `mcp_dispatch.rs` — `McpToolDispatch`: shared MCP tool definitions + dispatch (no rmcp dependency)
  - `error.rs` — shared error types
  - `ops/` — typed ops subtree: ack_deadline, admin, channel, claim, inbox, inventory, message, subscription, task, thread
- `crates/agent-bus-cli` (package `agent-bus`, binary `agent-bus`)
  - owns `lib.rs`, `cli.rs`, `commands.rs`, `server_mode.rs`, `monitor.rs`, `mcp_discovery.rs`
  - entry point: `main.rs` → `main_entry()` in `lib.rs`
  - integration tests under `crates/agent-bus-cli/tests/`
  - Note: still carries private shim modules re-exporting from `agent-bus-core`
- `crates/agent-bus-http` (package `agent-bus-http`, binary `agent-bus-http`)
  - owns `http.rs` (Axum router, SSE, MCP-HTTP bridge via `McpToolDispatch`)
  - `main.rs` calls `bootstrap()` from core directly
- `crates/agent-bus-mcp` (package `agent-bus-mcp`, binary `agent-bus-mcp`)
  - owns `mcp.rs` (`AgentBusMcpServer`: rmcp `ServerHandler` wrapping `McpToolDispatch`)
  - `main.rs` calls `bootstrap()` from core directly

### Transport Modes

- **MCP stdio** (`serve --transport stdio`): For LLM tool integration (Claude/Codex/Gemini MCP configs)
- **MCP Streamable HTTP** (`serve --transport mcp-http --port 8401`): MCP 2025-06-18 spec transport
- **HTTP REST** (`serve --transport http --port 8400`): Server mode with SSE streaming at `/events`
- **CLI**: Direct Redis/PG access for scripting and agent subprocesses

### CLI Subcommands

`health`, `send`, `read`, `watch`, `ack`, `presence`, `presence-list`, `serve`, `prune`, `export`, `presence-history`, `journal`, `sync`, `monitor`, `batch-send`, `pending-acks`, `claim`, `renew-claim`, `release-claim`, `claims`, `resolve`, `knock`, `codex-sync`, `service`, `post-direct`, `read-direct`, `post-group`, `read-group`, `session-summary`, `dedup`, `token-count`, `compact-context`, `push-task`, `pull-task`, `peek-tasks`

### MCP Tool Names (17 tools for stdio and mcp-http transports)

`bus_health`, `post_message`, `list_messages`, `ack_message`, `set_presence`, `list_presence`, `list_presence_history`, `negotiate`, `create_channel`, `post_to_channel`, `read_channel`, `claim_resource`, `renew_claim`, `release_claim`, `resolve_claim`, `knock_agent`, `check_inbox`. See [`AGENT_COMMUNICATIONS.md`](AGENT_COMMUNICATIONS.md) for full parameter docs.

### Channel System (v0.4)

Structured communication beyond broadcast:
- **Direct**: `POST /channels/direct/:agent` — private agent-to-agent with delivery confirmation
- **Group**: `POST /channels/groups/:name/messages` — named group discussions with member lists
- **Escalate**: `POST /channels/escalate` — auto-routes to orchestrator, priority=high
- **Arbitrate**: `POST /channels/arbitrate/:resource` — ownership claims with priority arguments

### Token-Optimized Features

- **TOON encoding** (`--encoding toon`): 70% token savings vs JSON. Format: `@from→to #topic [tags] body`
- **LZ4 compression**: Bodies >512 bytes auto-compressed, transparent decompression on read
- **Batch operations**: `/messages/batch`, `/read/batch`, `/ack/batch` for bulk operations

### Message Schema Validation

Use `--schema finding|status|benchmark` on `send` to validate message structure:
- **finding**: Requires `FINDING:` + `SEVERITY:`, or `FIX`/`COMPLETE` keywords
- **status**: Non-empty body
- **benchmark**: Contains key=value or key:value metrics

### Storage

**Redis** (realtime, always required): Streams (MAXLEN~100000), Pub/Sub, Presence (TTL), AOF persistence
**PostgreSQL** (durable history): Auto-creates tables, GIN index on tags, circuit breaker (60s cooldown)

On the Linux hub both are system services; the opt-in root hardening (Redis AOF start preflight, daily `pg_dump`) is `packaging/backend-hardening.sh` (also reachable as `./install.sh --with-backend-hardening`), see [`packaging/README.md`](packaging/README.md#backend-hardening-root-opt-in).

## Environment Variables

| Variable | Default | Source |
|----------|---------|--------|
| `AGENT_BUS_CONFIG` | `~/.config/agent-bus/config.json` | Config file path |
| `AGENT_BUS_REDIS_URL` | `redis://localhost:6380/0` | config.json |
| `AGENT_BUS_DATABASE_URL` | `postgresql://postgres@localhost:5300/redis_backend` | config.json |
| `AGENT_BUS_SERVER_HOST` | `localhost` (binds `127.0.0.1`; set `::1` for IPv6) | config.json |
| `AGENT_BUS_STREAM_MAXLEN` | `100000` | config.json |
| `AGENT_BUS_SESSION_ID` | (none) | env only — auto-tags messages with `session:<id>` |
| `AGENT_BUS_SERVER_URL` | (none) | config.json — single remote hub URL; CLI/MCP route through it instead of local Redis/PG |
| `AGENT_BUS_SERVER_URLS` | (none) | config.json `server_urls` — ordered, comma-separated hub candidates (#78). Takes priority over `AGENT_BUS_SERVER_URL` as a whole tier; `AGENT_BUS_SERVER_URL`/`server_url` remain a single-entry back-compat alias for `server_urls.first()`. A roaming client (e.g. a laptop off the fleet LAN) lists several reachable-path candidates (p2p, LAN, tailnet, ...) in priority order |
| `RUST_LOG` | `error` | env only |

### Remote-hub resolution (#78)

`agent_bus_core::hub::resolve_hub` tries `server_urls` in order (short connect
timeout per candidate) and returns exactly one of:

- **`Remote { url, authoritative, hub_build, tried }`** — a candidate
  answered `/health`. `authoritative` is `true` only for the first
  (highest-priority) candidate; anything else is a reachable fallback.
- **`Offline { tried }`** — candidates are configured but none answered.
  Writes (`send`, `presence`, `claim`, `ack`) are refused with a loud,
  actionable error — **never** silently applied to a local store. An
  exclusive `claim` specifically returns `"claim pending: no authoritative
  hub reachable"` rather than a hard failure or a local grant, since a
  caller may reasonably retry once a hub is reachable again.
- **`Local`** — no candidates configured at all (e.g. this process runs on
  the hub host itself). This is deliberate local-only mode, not a fallback,
  and is reported as such.

The CLI honors the same routing for the thread and ack-sweep commands
(#165): `thread-create`, `thread-join`, `thread-leave`, `thread-list`,
`thread-close`, `summarize-thread`, `compact-thread`, `pending-acks` and
`overdue-acks` call the hub's `/threads`, `/thread-summary`, `/compact-thread`,
`/pending-acks` and `/overdue-acks` endpoints whenever `server_urls` is
non-empty, and report the hub's error (or the offline error) instead of a
loopback Redis failure. `hub_routing_test.rs` pins Redis to a closed port, so
a regression that opens local Redis in remote mode fails there.

`bus_health` (CLI `health`, MCP `bus_health` tool) always includes a
`backend` field reporting exactly which of the three states above answered,
so a client silently reading its own local store while believing it talks to
the fleet (the original #78 "island" symptom) is detectable from the health
output alone. `agent-bus-mcp` (stdio) proxies every other tool call to the
resolved remote hub via a blocking-from-async `RemoteMcpTransport`
(`agent_bus_core::remote_dispatch::RoutingDispatch`); `agent-bus-http` (the
hub itself) never uses this path — it always dispatches locally, so a hub
can never proxy to itself even if its own config named a `server_url`.

#### Link-aware selection within ordered stores

`agent_bus_core::link_route` races interchangeable routes for the CLI and the
stdio MCP server (`resolve_*_hub_concurrent`; the closure-based
`resolve_configured_hub` / `resolve_authoritative_hub` remain as the
deterministic serial walk over the same per-store plans):

- **Store barrier**: exhaust authoritative stores before independent fallback
  stores, then cloud. Within a tier, stores retain their first configured
  position. Only the same role plus the same explicit, nonempty `hub` name
  groups aliases; unnamed URLs are separate stores. A cached or faster cloud
  route cannot bypass a healthy authority.
- **Class** per candidate (`LinkClass::of`): `fabric` (a `sites` word of
  `fabric`, `tb`, `thunderbolt`, `usb4`, `p2p`) > `lan` (everything else,
  including direct links and SSH-forwarded loopback) > `tailnet` (site
  `tailnet`/`tailscale`, `*.ts.net`, or `100.64.0.0/10`) > `cloud` (role).
- **Candidate set**: routes whose `sites` intersect the detected
  `network_locations`, plus site-less, tailnet and cloud routes. Unknown or
  undetected location keeps every route eligible. Routes declared for another
  location form that store's *deferred* stage. It is exhausted after the
  primary stage fails and before any independent store is considered, so a
  wrong location guess never permanently discards a declared route.
- **Race within a store**: each route's `/health` probe starts after `wave *
  probe_stagger_ms` (default 75 ms, `AGENT_BUS_PROBE_STAGGER_MS`), where a wave
  is one (link class, role) pair: authoritative routes lead fallbacks on the
  same kind of link, and routes of equal class and role start together so the
  lower-RTT one wins. Each probe is bounded by
  `probe_connect_timeout_ms` (default 750 ms). The first healthy answer that
  passes the unchanged identity, credential and authority checks wins; the rest
  are dropped. Claims consider only `authoritative` routes.
- **Route cache** (`hub-resolution.json`, TTL `hub_cache_ttl_seconds`, unchanged
  default 60 s): winner URL, role and RTT, keyed by candidate list, location set and a
  fingerprint of the local interface prefixes. A changed attachment key makes
  a newly run resolver re-probe. A cached alias is still health-probed first
  inside its own store group (no stagger); it cannot reorder independent
  stores. Failed remote calls invalidate the route cache. Successful resolutions
  refresh its timestamp, so this TTL is not a guaranteed periodic race under
  continuous use. The key ignores duplicate prefixes: a newly added interface
  sharing an existing prefix need not invalidate it.
- **MCP backend-cache limit**: `remote_dispatch.rs` also retains a resolved
  backend for up to the existing 15 seconds, keyed only by candidates. A
  network change can therefore wait for that cache to expire before the inner
  route resolver sees the new attachment key. Actual call failure invalidates
  both caches immediately; cache invalidation itself does not replay a call.
- **`health`** keeps `backend.tried` and adds `backend.probes[]`
  (`{url, class, rtt_ms | error}`) and `backend.selected`. No secrets.

The hub-to-cloud sync (`cloud_sync.rs`) dials a single validated `cloud_url`,
so there is no candidate set to select over; it does not use this selector.
See [routing and cache limits](docs/network-location-routing.md) and the
[resolver](crates/agent-bus-core/src/hub.rs) for the consistency boundary.

A durable offline write outbox (spool-and-replay for `send`/`presence`/
`claim`/`ack` while offline, with idempotent UUIDv7 client ids) is
deliberately **not** implemented yet — see #80.

**PR #81 review follow-up (2026-09-27), scope limits:** an `https://`
candidate (e.g. a Cloudflare-hosted tier per #79) now works — both
`agent-bus-cli` and `agent-bus-mcp` link `reqwest`'s `rustls` feature (NOT
`rustls-tls`, which doesn't exist in reqwest 0.13; `default-tls =
["rustls"]` there), using `rustls-platform-verifier` for the OS trust store
on Linux and Windows rather than a hardcoded webpki root bundle. This only
covers the CLIENT side (dialing out to a remote hub); `agent-bus-http` (the
hub binary itself) does not terminate TLS directly. `resolve_server_url_list`
also deduplicates `server_urls` by exact string match (first occurrence
wins — no trailing-slash/case normalization), and `AGENT_BUS_SERVER_URLS=""`
(present but empty) is confirmed to fall through to the next tier exactly
like the var being unset, with no "explicit local-only override" sentinel.
The #80 outbox above remains the one deliberately deferred gap. Separately,
`codex/fix-filtered-inbox`'s `8a2064e` (filtered-inbox pagination, part of
#77) predates and is orthogonal to #78's routing scope — it was carried
through the rebase described in PR #81 but was not itself re-reviewed as
part of #78/#81 and should get its own independent review.

## MCP Platform Configs

All 3 platforms configured identically at:
- Claude Code: `~/.claude.json` top-level `mcpServers["agent-bus"]`
- Codex: `~/.codex/config.toml` (key: `agent_bus`)
- Gemini: `~/.gemini/settings.json` (key: `agent-bus`)

Example configs for all platforms live in `examples/mcp/`.

## Git Hooks

Lefthook (install with `lefthook install`):
- **pre-commit** (parallel): `cargo fmt --check`, `cargo clippy` (the old `ast-grep scan` step was removed in #64 — it invoked shadow-utils `sg`, never ast-grep)
- **pre-push**: `cargo test` (live-bus env pinned to a closed port), `cargo audit` — **blocking**; a missing `cargo-audit` is a failure, not a skip
- Until #64 every command carried `root: "."`, which under lefthook 2.1.9 made them all silently skip; CI now fails if any lefthook command runs nothing (#65)
- **commit-msg**: conventional commit format advisory

## Rust Conventions

### Owned disposable integration fixtures

Use `python3 -B scripts/ci/isolated-services.py integration` for the entire
workspace backend suite and `python3 -B scripts/ci/isolated-services.py history`
for the history migration contract. Both require a local Docker daemon;
containerized CI explicitly selects its runner container with
`--network-container <container-id>`. The history contract creates a fresh
container and never creates or drops a database on the fleet PostgreSQL port.
The harness clears inherited agent-bus settings, requires service/backend
identity plus authenticated administration before mutations, and cleans only
captured fixture IDs and its owned HTTP child. Run its pure regressions with
`python3 -B -m unittest discover -s scripts/ci -p 'test_*.py'`.

- **Allocator**: mimalloc per M-MIMALLOC-APPS
- **Error handling**: `anyhow::Result` with `.context()` — no unwrap in business logic
- **Lints**: Clippy pedantic + restriction subset in workspace root `Cargo.toml [workspace.lints]`
- **Edition**: 2024

## Merging: the merge autopilot

The fleet's merge autopilot merges PRs here; do not merge by hand or start a second scheduler, and do not use `--admin`:
only the operator authorizes a bypass, on agent-bus thread `machine-hygiene`.

- **Open the PR and let it land.** The autopilot reviews each new head, updates `behind` branches, and squash-merges
  when the required checks pass at the exact head and its review gate admits the PR.
- **One concern per PR.** A review covers one diff and carries over to later heads with a byte-identical PR diff, so
  small PRs review and merge fastest.
- **Read your PR's route.** The `merge-autopilot/admission` commit status description names who must do what next. The
  `autopilot-escalated` label marks an overdue route. The autopilot's `decisions.jsonl` and `state.json`, in its state
  directory (default `~/.local/state/merge-autopilot` on Linux), hold the detail.
- **Independence.** A claude-authored PR needs a codex approval, and a codex-authored PR needs a claude approval.
  HIGH-risk paths (CI, deploy, hooks, hardware and release code) need two approvals, one from outside the codex family.
- **Blockers** go on agent-bus thread `machine-hygiene`, tagged `repo:<name>`.
- Operation, onboarding and troubleshooting: [merge-autopilot README](https://github.com/David-Martel/vigil-utils/blob/main/tools/merge-autopilot/README.md).
