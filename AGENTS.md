# Repository Guidelines

## Project Structure & Module Organization

The top-level [Cargo workspace](./Cargo.toml) contains exactly four crates;
`rust-cli/` has been removed.

- `crates/agent-bus-core`: shared storage, validation, models, typed ops,
  bootstrap, and MCP dispatch.
- `crates/agent-bus-cli` (package `agent-bus`): CLI commands, server-mode bridge,
  inline HTTP/MCP `serve`, benches, and CLI/HTTP integration tests.
- `crates/agent-bus-http`: long-running HTTP/SSE service and MCP-HTTP endpoint.
- `crates/agent-bus-mcp`: dedicated MCP stdio server.

The crate split is complete. The CLI still links transport dependencies for
inline `serve` and carries re-export shims; surface thinning remains open in
[`TODO.md`](./TODO.md). Use the [README crate map](./README.md#crate-map) to
locate a contribution and [`agents.TODO.md`](./agents.TODO.md) for structural
work. Dated refactor plans describe their historical checkpoints.

Supporting material remains split across `scripts/` for PowerShell automation,
`examples/mcp/` for client configs, and `docs/` for design notes, assessments,
status snapshots, and agent templates.

Canonical structural refactor plan:
- [`agents.TODO.md`](./agents.TODO.md)

Dated post-split architecture snapshot:
- [`docs/current-status-2026-06-13.md`](./docs/current-status-2026-06-13.md)
  (test counts and runtime observations are historical; use current source and
  fresh receipts for validation).

## Build, Test, and Development Commands

Run from the repository root with the compiler pinned by
[`rust-toolchain.toml`](./rust-toolchain.toml) (currently Rust 1.98.1). Linux
uses native Cargo; Windows build automation uses PowerShell. On deployment
hosts, run ordinary QA in an isolated disposable container with source mounted
read-only; do not inherit live bus configuration or credentials.

- `cargo build --release -p agent-bus -p agent-bus-http -p agent-bus-mcp`: build the three shipping binaries.
- `bash scripts/test-agent-bus-isolated.sh`: canonical pre-push workspace test entry point; sanitizes inherited configuration and pins stores to closed ports.
- `cargo test --workspace --lib --bins`: CI's unit selection, with the isolation settings in its `test-unit` job. No live backend is required.
- `cargo test -p agent-bus --lib <test_filter>`: select CLI command library unit tests inside the same isolated environment. `--bin agent-bus` does not select those library tests.
- `cargo test --workspace --tests -- --ignored --test-threads=1` at repo root: run the `#[ignore]`d backend tests. They require `AGENT_BUS_TEST_REDIS_URL`, `AGENT_BUS_TEST_DATABASE_URL` and `AGENT_BUS_TEST_SERVER_URL` pointing at DISPOSABLE backends and fail (never skip) when those are unset or unreachable; the live bus ports 6380/5300/8400 are refused. CI's `test-integration` job shows how to start them.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings`: match CI formatting and lint gates.
- `cargo check -p agent-bus --no-default-features`: CI's minimal CLI feature check.
- `pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease`: repo-root fast iteration build using the shared target-dir, linker, and `sccache` setup.

[`ci.yml`](./.github/workflows/ci.yml) and
[`release.yml`](./.github/workflows/release.yml) define the current Linux X64,
Linux ARM64, Windows X64, and Docker validation/artifact jobs. The
[fleet runner guide](./docs/fleet-build-runners.md) describes routing and trust
limits. Backend tests must use disposable services, never the fleet bus.

## Coding Style & Naming Conventions

Use Rust 2024 edition defaults with `rustfmt` width 100 and field init shorthand enabled. Follow Clippy strictly; CI and hooks treat warnings as failures. Use `snake_case` for modules and functions; `CamelCase` only for types. Keep new PowerShell automation in `scripts/` with verb-noun names such as `build-deploy.ps1`.

## Testing Guidelines

Place CLI/HTTP integration coverage in `crates/agent-bus-cli/tests/*_test.rs`;
shared unit coverage belongs beside the affected module in
`crates/agent-bus-core/src/`. `http_integration_test.rs` contains real backend
tests. Disposable loopback controls such as `hub_routing_test.rs` run without
external services; ignored backend tests run in CI's `test-integration` job
against its disposable Redis/PostgreSQL/HTTP instances and fail when their
required endpoints are unavailable. Derive counts from the exact test run,
including selected/passed/ignored totals, rather than copying dated inventories.
No fixed coverage percentage is enforced, but every feature change should add
or update tests in the affected runtime. Prefer focused unit tests first, then
integration coverage for Redis/PostgreSQL behavior, HTTP endpoints, and MCP
behavior when transport semantics change.

## Commit & Pull Request Guidelines

Use conventional commits with optional scopes, matching recent history: `feat(http): ...`, `perf(pg): ...`, `docs: ...`, `chore: ...`. Install hooks with `lefthook install`; pre-commit runs `fmt` and `clippy`, and pre-push runs `cargo test` and a blocking `cargo audit`. PRs should describe behavior changes, note required local services or env vars, link issues when applicable, and include screenshots only for dashboard/UI changes.

## Automated Agents (Jules)

Jules (Google's async coding agent) reads this file. It runs in a Google-hosted VM, so:

- **Not available in the VM:** the live bus (Redis 6380, PostgreSQL 5300, HTTP 8400), the fleet
  LAN, Windows, and `pwsh`. For `scripts/*.ps1` work, fetch the PowerShell 7 linux-x64 release
  tarball from GitHub (apt/snap installs fail in the VM), then
  `pwsh -c 'Install-Module Pester,PSScriptAnalyzer -Force -Scope CurrentUser'`. Never point a test at a live bus port. The `#[ignore]`d
  backend tests need disposable backends; if you cannot start them, say so and skip them.
- **Run these (no services needed):** `cargo fmt --all --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, and
  `bash scripts/test-agent-bus-isolated.sh` at the repo root. Report exactly what you ran and the
  result. Never guess a result you could not run.
- **Review-only tasks** (the prompt says so): do not commit, push or open a PR. End the session
  with the findings as your final message: numbered, each with SEVERITY, file:line, failure
  scenario and a suggested fix.
- **Change tasks:** one concern per PR, conventional-commit subject, and a commit trailer
  `Agent: jules`. Do not bump the bus protocol or wire formats: every fleet host runs the same
  `agent-bus` build.
- **Do not ask for confirmation.** State any assumption and continue. When the task is done,
  finish; do not stop to ask "should I proceed?".

## Learned Backend Lifecycle Rules (append-only)

- **[2026-10-05] HTTP shutdown is not managed-process termination.** A successful HTTP stop/flush response and a closed listener prove those operations only. Before declaring a managed hub stopped, verify its supervisor/service state and process exit independently; use the owning service manager to complete termination when the process remains active. Preserve flush/readback evidence and the first failure. Do not attribute a lingering process to SSE without connection or lifecycle evidence. A failed server-admin status request must fail visibly rather than return success with `admin: null`; Windows SCM-only and builds without server-mode must identify their narrower evidence tier.
