# Structural TODO Completion Plan

This file is the canonical execution plan for the remaining structural
refactor work. It is intentionally narrower than the main roadmap in
[`TODO.md`](./TODO.md): it focuses only on the architecture, crate/package
layout, transport boundaries, and validation work required to finish the
structural simplification of the Rust implementation.

## Current State

Already complete:

- The runtime is Rust-only.
- The active package has a library-backed root in `rust-cli/src/lib.rs`.
- The CLI server-mode bridge is async and no longer relies on
  `reqwest::blocking`.
- A first shared `ops` layer now owns message send/ack/knock/presence flows.
- The package emits dedicated `agent-bus`, `agent-bus-http`, and
  `agent-bus-mcp` binaries from one codebase.
- The local build pipeline is centralized and now uses checked-in cargo
  configuration, `sccache` auto-detection, and stable Windows linker defaults.

Still structurally incomplete:

- The shared service layer is still too small. Large parts of `commands.rs`,
  `http.rs`, and `mcp.rs` still duplicate orchestration, validation glue, and
  output shaping.
- The codebase is still one crate/package, so all transport surfaces pay for
  each other's compile graph and dependency set.
- Some transport-facing logic is stranded in the wrong layer, especially inbox
  cursor handling, channel orchestration, claim flows, and maintenance/admin
  behavior.

## Primary Goals

1. Expand the typed shared service layer until CLI, HTTP, and MCP become thin
   parse-call-render adapters.
2. Split the monolithic package into reusable crates without breaking existing
   operator-facing binary names or scripts.
3. Keep local build, deploy, bootstrap, and smoke flows stable during the
   refactor.
4. Preserve behavior parity while reducing cross-surface duplication.

## Non-Goals

- Do not redesign the wire protocol during the structural refactor.
- Do not replace Redis or PostgreSQL abstractions first.
- Do not move storage primitives out of `redis_bus.rs` / `postgres_store.rs`
  until the higher-level service boundaries are stable.
- Do not block operator workflows on a perfect workspace split; sequence the
  refactor so each phase is independently shippable.

## Architectural Target

End state:

- `agent-bus-core`
  - Models
  - Settings
  - Validation
  - Token/minimization helpers
  - Shared service layer (`ops`)
  - Redis/PostgreSQL infrastructure adapters
- `agent-bus-cli`
  - Clap parser
  - CLI-specific presentation/output
  - Thin command adapters
- `agent-bus-http`
  - Axum server
  - SSE and HTTP-specific DTOs/handlers
  - Thin handler adapters
- `agent-bus-mcp`
  - MCP schemas and transport
  - Thin tool dispatch adapters

Compatibility rules:

- Keep the installed `agent-bus.exe` command as the main operator CLI.
- Keep `agent-bus-http.exe` as the Windows service/runtime surface.
- Keep `agent-bus-mcp.exe` as the dedicated stdio MCP surface.
- Preserve existing command names, HTTP routes, and MCP tool names unless a
  dedicated migration note is added first.

## Phase 1: Expand The Shared Service Layer

Objective:

- Move transport-agnostic orchestration out of `commands.rs`, `http.rs`, and
  `mcp.rs` into typed operations modules.

Target shape:

- `rust-cli/src/ops/message.rs`
- `rust-cli/src/ops/channel.rs`
- `rust-cli/src/ops/claim.rs`
- `rust-cli/src/ops/task.rs`
- `rust-cli/src/ops/inbox.rs`
- `rust-cli/src/ops/admin.rs`
- `rust-cli/src/ops/mod.rs`

### Phase 1A: Message and inbox completion

- Move notification cursor logic out of `mcp.rs` and into `ops/inbox`.
- Add typed request/response structs for:
  - list inbox
  - check inbox
  - compact context input
  - thread-scoped history reads
  - pending ack reads
- Keep rendering concerns outside the ops layer.

### Phase 1B: Channels and claims

- Move direct/group/escalation/arbitration orchestration behind typed channel
  operations.
- Move claim/renew/release/resolve/list logic behind typed claim operations.
- Keep low-level Redis stream/hash logic in `channels.rs` initially, but route
  all transport entrypoints through typed ops requests.

### Phase 1C: Tasks and admin control

- Move task queue reads/writes behind typed task operations.
- Extract service-control/maintenance orchestration into `ops/admin`.
- Keep `server_mode.rs` transport-only. It should remain an HTTP client wrapper,
  not a home for business logic.

Exit criteria:

- `commands.rs` primarily performs argument parsing, schema selection,
  transport-mode routing, and output formatting.
- `http.rs` primarily performs HTTP DTO extraction, spawn boundaries, and
  response conversion.
- `mcp.rs` primarily performs MCP schema wiring and output conversion.

## Phase 2: Normalize Transport Boundaries

Objective:

- Make the three surfaces structurally consistent so they can be split into
  separate crates with minimal logic movement.

Tasks:

- Introduce a stable pattern for request/response DTO naming across ops.
- Remove transport-specific validation that can be shared safely.
- Consolidate JSON shaping so HTTP and MCP do not each re-encode the same
  semantic results differently without reason.
- Isolate output/presentation helpers that are CLI-only.

Required code moves:

- Keep `output.rs` CLI-facing.
- Keep HTTP response-only structs in `http.rs` or an HTTP-local module.
- Keep MCP schema declarations in `mcp.rs` or an MCP-local schema module.
- Move only semantic result types into the shared layer.

Exit criteria:

- Cross-transport behavior changes land in one ops module first.
- `commands.rs`, `http.rs`, and `mcp.rs` no longer contain parallel copies of
  the same orchestration logic for claims, channels, inbox reads, and admin
  control.

## Phase 3: Split The Package Into Crates

Objective:

- Reduce compile/link coupling and make each deployment surface pay only for the
  dependencies it actually uses.

Recommended sequence:

1. Create a top-level Cargo workspace.
2. Move shared modules into `crates/agent-bus-core`.
3. Create `crates/agent-bus-cli`.
4. Create `crates/agent-bus-http`.
5. Create `crates/agent-bus-mcp`.
6. Keep compatibility binaries/wrappers as needed until scripts and configs are
   fully migrated.

Workspace responsibilities:

- `agent-bus-core`
  - no clap
  - no axum presentation layer
  - no MCP schema definitions
- `agent-bus-cli`
  - clap
  - CLI formatting/output
- `agent-bus-http`
  - axum
  - SSE
  - HTTP admin routes
- `agent-bus-mcp`
  - rmcp
  - MCP tool schemas

Migration rules:

- Move by stable module seams, not by large file dumps.
- Prefer re-export shims during the intermediate state to keep scripts and
  tests running.
- Land the workspace split only after Phase 1 and Phase 2 have already reduced
  logic churn.

Exit criteria:

- Each surface builds independently against a shared core.
- HTTP/MCP-specific dependencies are not dragged into pure CLI-only builds.
- Local scripts know where to find the new artifact paths without guessing.

## Phase 4: Build, Script, And Packaging Cleanup

Objective:

- Make the workspace split invisible to operators.

Tasks:

- Update `build.ps1`, `validate-agent-bus.ps1`, `build-deploy.ps1`,
  `setup-agent-hub-local.ps1`, and `bootstrap.ps1` to target the new crate
  layout explicitly.
- Keep repo-root cargo aliases authoritative.
- Preserve `sccache` behavior and linker defaults in the checked-in cargo
  config.
- Ensure Windows service install/update scripts point to the HTTP crate output.
- Ensure MCP install scripts prefer `agent-bus-mcp.exe`.

Exit criteria:

- New-machine bootstrap still works from the repo root.
- Local operator workflows do not need to know internal crate names.
- Service deploy/update flows keep working during and after the split.

## Phase 5: Validation Matrix

Objective:

- Prove the refactor did not create surface-specific drift.

Required validation:

- Shared layer unit tests per ops module.
- CLI/HTTP/MCP parity tests for:
  - send
  - ack
  - presence
  - inbox/check_inbox
  - claim/renew/release/resolve
  - direct/group channel operations
  - compact-context and future thread-summary flows
- CLI server-mode tests against `AGENT_BUS_SERVER_URL`.
- PostgreSQL regression tests for all read/compaction paths that bind JSON/JSONB
  metadata.
- Mixed multi-repo tests that verify repo/session/thread scoping prevents
  inbox bleed-through.

Recommended local gating order:

1. `cargo fmt --all --check`
2. `cargo clippy --all-targets -- -D warnings`
3. focused unit tests for changed ops modules
4. parity tests
5. integration tests
6. local smoke harnesses

## Phase 6: Documentation Completion

Objective:

- Keep the refactor legible to humans and other agents.

Required doc updates per milestone:

- [`TODO.md`](./TODO.md): status only, concise.
- [`README.md`](./README.md): operator-facing runtime/build/deploy expectations.
- [`AGENTS.md`](./AGENTS.md): terse repo guidance and the canonical structural
  plan link.
- [`CLAUDE.md`](./CLAUDE.md): updated build/test/binary-role guidance for Claude
  workflows.
- [`MCP_CONFIGURATION.md`](./MCP_CONFIGURATION.md): exact MCP surface guidance.

When Phase 3 starts:

- Add an explicit crate map to `README.md`.
- Add migration notes for any changed artifact paths.
- Add a one-paragraph “how to find the right crate” note for agents.

## Recommended Execution Order

Do the remaining work in this order:

1. Finish Phase 1A inbox/message shared ops.
2. Finish Phase 1B claim/channel shared ops.
3. Finish Phase 1C task/admin shared ops.
4. Normalize transport boundaries in CLI/HTTP/MCP.
5. Add parity tests around the extracted ops.
6. Split into a workspace and move the core crate first.
7. Move CLI, then HTTP, then MCP.
8. Update scripts/bootstrap/install flows.
9. Refresh docs and public-release notes.

## Risk Register

Highest-risk points:

- HTTP and MCP response drift during ops extraction.
- Breakage in service deploy scripts during artifact path changes.
- Hidden coupling between `commands.rs` and transport-local JSON shaping.
- Test coverage gaps around notification cursors and PostgreSQL fallback paths.

Mitigations:

- Keep each phase shippable.
- Add parity tests before moving crates.
- Avoid simultaneous storage-module refactors.
- Keep binary names stable even if crate names change underneath.

## Definition Of Done

The structural TODO work is complete when all of the following are true:

- Shared service logic for message, inbox, channel, claim, task, and admin
  flows lives in typed ops modules or their equivalent core-layer successors.
- CLI, HTTP, and MCP are thin surfaces over that shared layer.
- The workspace split is complete and stable.
- Local build/deploy/bootstrap/install scripts target the split layout
  correctly.
- Clippy, focused unit tests, parity tests, and local smoke harnesses pass
  against the new structure.
- `TODO.md` can remove the remaining structural bullets in favor of normal
  feature work.
