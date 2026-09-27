# Repository Guidelines

## Project Structure & Module Organization

The Cargo workspace contains `crates/agent-bus-core` (shared storage and
operations), `crates/agent-bus-cli` (package `agent-bus`),
`crates/agent-bus-http`, and `crates/agent-bus-mcp`. The former `rust-cli/`
directory has been removed. CLI integration targets live under
`crates/agent-bus-cli/tests/`; service harnesses live under `scripts/ci/`.

Supporting material remains split across `scripts/` for PowerShell automation,
`examples/mcp/` for client configs, and `docs/` for design notes, assessments,
status snapshots, and agent templates.

Canonical structural refactor plan:
- [`agents.TODO.md`](./agents.TODO.md)

Code-grounded status snapshot:
- [`docs/current-status-2026-04-03.md`](./docs/current-status-2026-04-03.md)

## Build, Test, and Development Commands

- `cargo build --release --workspace --bins` at repo root: build the shipping binaries.
- `cargo test --workspace --lib --bins` at repo root: fast code-grounded check across the workspace without requiring live Redis/HTTP services.
- `cargo test -p agent-bus --lib --bins`: run CLI unit tests.
- `python scripts/ci/isolated-services.py integration` at repo root: run all four CLI integration targets against disposable Docker services, including ignored tests.
- `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings` at repo root: match CI formatting and lint gates.
- `pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease`: repo-root fast iteration build using the shared target-dir, linker, and `sccache` setup.

Do not point integration tests at the shared Redis, PostgreSQL, or HTTP bus.
The harness requires Python 3.10+, Cargo and a local Docker engine providing
Linux containers. Missing prerequisites fail; there is no live-service fallback.
It starts fresh Redis/PostgreSQL containers on random loopback ports, launches
its own HTTP binary, and verifies process/backend identity before testing.
All four targets (including `cli_http_parity_test`) run with `--include-ignored`
and `--test-threads=1`. The live tests are ignored in ordinary `cargo test`;
`cargo test -p agent-bus --test integration_isolation_test` checks the isolation
guard without services. Pre-push runs that guard plus workspace library/binary
tests. CI also runs the disposable integration harness; unit tests alone are
not evidence of service integration success.

Use `python scripts/ci/isolated-services.py history` for the history migration
contract in a fresh disposable PostgreSQL database. The explicit Cargo target
must exist; missing targets and test failures are errors. No shared database is
created or dropped. For release smoke, use `python scripts/ci/isolated-services.py
smoke --cli <artifact-path> --http <artifact-path>` (also requires PowerShell 7).
The harness owns the HTTP subprocess; the maintained smoke script checks the
existing server's PID, identity and backends and requires healthy PostgreSQL.

The harness clears inherited `AGENT_BUS_*` settings except build provenance,
uses an empty temporary configuration file, disables startup announcements,
and supplies `AGENT_BUS_TEST_RUN_ID`, `AGENT_BUS_TEST_REDIS_URL`,
`AGENT_BUS_TEST_DATABASE_URL`, `AGENT_BUS_TEST_HTTP_URL`, and
`AGENT_BUS_TEST_AUTH_TOKEN`. These variables are an internal harness contract,
not instructions to adapt production endpoints to tests. Cleanup uses only
captured container IDs and owned process handles. A failed cleanup is reported;
there is no guessed-name/stale-resource deletion. Do not publish generated
passwords or bearer tokens. Test the harness itself without services using
`python -B -m unittest discover -s scripts/ci -p "test_*.py"`.

## Coding Style & Naming Conventions

Use Rust 2024 edition defaults with `rustfmt` width 100 and field init shorthand enabled. Follow Clippy strictly; CI and hooks treat warnings as failures. Use `snake_case` for modules and functions; `CamelCase` only for types. Keep new PowerShell automation in `scripts/` with verb-noun names such as `build-deploy.ps1`.

## Testing Guidelines

Place CLI integration coverage in `crates/agent-bus-cli/tests/*_test.rs`.
Shared unit coverage lives under `crates/agent-bus-core/src/*`. Integration
tests requiring external services must be ignored by default and validate
the harness contract before any backend access.
No fixed coverage percentage is enforced, but every feature change should add
or update tests in the affected runtime. Prefer focused unit tests first, then
integration coverage for Redis/PostgreSQL behavior, HTTP endpoints, and MCP
behavior when transport semantics change.

## Commit & Pull Request Guidelines

Use conventional commits with optional scopes, matching recent history: `feat(http): ...`, `perf(pg): ...`, `docs: ...`, `chore: ...`. Install hooks with `lefthook install`; pre-commit runs workspace `fmt`, `clippy`, and `ast-grep` for nested Rust changes; pre-push runs service-free workspace and isolation-guard tests. PRs should describe behavior changes, provide isolated integration results when relevant, link issues when applicable, and include screenshots only for dashboard/UI changes.
