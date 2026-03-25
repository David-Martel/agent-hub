# Structural Refactor Plan

## Completed Baseline

- Rust runtime only; Python transport/codecs removed.
- Shared build helper, `fast-release`, cargo aliases, and repo cleanup landed.
- Repo-local cargo config now auto-enables `sccache` through a checked-in wrapper when the tool is available.
- `main.rs` is now a thin wrapper over a library-backed runtime entry.
- Dedicated `agent-bus-http` and `agent-bus-mcp` bin targets now sit on top of that shared library runtime.
- CLI server-mode routing uses async `reqwest`, not `reqwest::blocking`.

## Remaining Structural Goals

- Extract a typed shared operations/service layer so CLI, HTTP, and MCP stop re-implementing the same Redis/PostgreSQL orchestration.
- Split the current monolithic package into reusable surfaces without duplicating runtime wiring.
- Reduce churn in `commands.rs`, `http.rs`, and `mcp.rs` by pushing transport-agnostic work behind typed requests.

## Phase Plan

### Phase 1: Shared Operations Module

Scope:
- Message send
- Message read/filter scope
- Ack send
- Presence update

Rules:
- Keep transport-specific validation and HTTP/MCP schema parsing in the transport modules.
- Move only transport-agnostic Redis/PostgreSQL orchestration into shared typed request functions.
- Preserve current response shapes so callers do not need wide rewrites.

Target modules:
- `rust-cli/src/ops.rs`
- `rust-cli/src/commands.rs`
- `rust-cli/src/http.rs`
- `rust-cli/src/mcp.rs`

Exit criteria:
- CLI, HTTP, and MCP all call the same typed send/read/ack/presence helpers.
- Filter/tag-scope helpers live outside `commands.rs`.

### Phase 2: Expand Shared Service Layer

Scope:
- Channel post/read helpers
- Claim/renew/release/resolve orchestration
- Service-control orchestration
- Task queue operations

Rules:
- Define typed request/response structs per capability.
- Keep direct Redis primitives private to the shared layer wherever practical.

Target modules:
- `rust-cli/src/channels.rs`
- `rust-cli/src/commands.rs`
- `rust-cli/src/http.rs`
- `rust-cli/src/mcp.rs`
- `rust-cli/src/server_mode.rs`

Exit criteria:
- Transport modules mostly translate arguments to typed requests and format outputs.
- Cross-transport behavior changes happen in one place.

### Phase 3: Split Package Surfaces

Scope:
- Create a reusable core crate for models, settings, shared ops, Redis/PostgreSQL access, and transport-neutral logic.
- Create thin surface crates/binaries for CLI, HTTP, and MCP.

Likely crate sequence:
1. `agent-bus-core`
2. `agent-bus-cli`
3. `agent-bus-http`
4. `agent-bus-mcp`

Rules:
- Preserve current command names and installed binary faces.
- Move code only after the shared service layer is stable enough to avoid bouncing logic between crates.

Exit criteria:
- CLI/HTTP/MCP compile against a shared core instead of one monolithic binary crate.
- Surface-specific dependencies stop inflating every build.

### Phase 4: Validation And Packaging

Scope:
- Add direct tests for shared ops.
- Add CLI/HTTP/MCP parity tests around the extracted flows.
- Tighten bootstrap/install docs for the split surfaces.

Exit criteria:
- Structural refactor does not regress transport parity.
- Local validation scripts still work against the split package layout.
