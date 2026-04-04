# Repository Guidelines

## Project Structure & Module Organization

The repo now has a top-level Cargo workspace with `rust-cli/` plus
`crates/agent-bus-core`, `crates/agent-bus-cli`, `crates/agent-bus-http`, and
`crates/agent-bus-mcp`.

Current code-grounded split status (2026-04-04):
- `agent-bus-core` owns extracted shared logic: storage adapters, validation,
  token helpers, channels, and typed ops (1,205 lines across 7 ops modules).
- `rust-cli/` remains the primary runtime crate and still owns `lib.rs`,
  `cli.rs`, `commands.rs` (1,685 lines), `http.rs` (2,836 lines),
  `mcp.rs` (1,471 lines), `server_mode.rs`, `mcp_discovery.rs`,
  `codex_bridge.rs`, benches, and integration tests.
- The surface crates currently wrap `rust-cli`; they are not yet fully
  independent implementations.
- `scripts/` still builds and deploys through `rust-cli/`.
- ~4,400 lines of business logic remain duplicated across transport surfaces;
  Phase 1 of `agents.TODO.md` targets consolidation into core ops.

Supporting material remains split across `scripts/` for PowerShell automation,
`examples/mcp/` for client configs, and `docs/` for design notes, assessments,
status snapshots, and agent templates.

Canonical structural refactor plan:
- [`agents.TODO.md`](./agents.TODO.md)

Code-grounded status snapshot:
- [`docs/current-status-2026-04-03.md`](./docs/current-status-2026-04-03.md)

## Build, Test, and Development Commands

- `cargo build --release` in `rust-cli/`: build the shipping CLI binary.
- `cargo test --workspace --lib --bins` at repo root: fast code-grounded check across the workspace without requiring live Redis/HTTP services.
- `cargo test --bin agent-bus` in `rust-cli/`: run Rust unit tests.
- `cargo test --test integration_test --test http_integration_test --test channel_integration_test -- --test-threads=1` in `rust-cli/`: run integration tests against local Redis and PostgreSQL.
- `cargo fmt --all --check` and `cargo clippy --all-targets -- -D warnings` in `rust-cli/`: match CI formatting and lint gates.
- `pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease`: repo-root fast iteration build using the shared target-dir, linker, and `sccache` setup.

Set local services with `AGENT_BUS_REDIS_URL` and `AGENT_BUS_DATABASE_URL` when running integration flows.

## Coding Style & Naming Conventions

Use Rust 2024 edition defaults with `rustfmt` width 100 and field init shorthand enabled. Follow Clippy strictly; CI and hooks treat warnings as failures. Use `snake_case` for modules and functions; `CamelCase` only for types. Keep new PowerShell automation in `scripts/` with verb-noun names such as `build-deploy.ps1`.

## Testing Guidelines

Place integration coverage in `rust-cli/tests/*_test.rs`. Shared unit coverage
for extracted logic now primarily lives under `crates/agent-bus-core/src/*`.
Current test inventory: 304 unit tests in `agent-bus-core`, 10 integration tests
in `rust-cli/tests/` (314 total). `http_integration_test.rs` is a skeleton
needing test functions.
No fixed coverage percentage is enforced, but every feature change should add
or update tests in the affected runtime. Prefer focused unit tests first, then
integration coverage for Redis/PostgreSQL behavior, HTTP endpoints, and MCP
behavior when transport semantics change.

## Commit & Pull Request Guidelines

Use conventional commits with optional scopes, matching recent history: `feat(http): ...`, `perf(pg): ...`, `docs: ...`, `chore: ...`. Install hooks with `lefthook install`; pre-commit runs `fmt`, `clippy`, and `ast-grep`, and pre-push runs `cargo test`. PRs should describe behavior changes, note required local services or env vars, link issues when applicable, and include screenshots only for dashboard/UI changes.
