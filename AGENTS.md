# Repository Guidelines

## Project Structure & Module Organization

The active implementation is the Rust CLI in `rust-cli/`, which contains the `agent-bus` binary, HTTP/MCP server, benches, and integration tests. Supporting material is split across `scripts/` for PowerShell automation, `examples/mcp/` for client configs, and `docs/` for design notes, assessments, and agent templates.

Canonical structural refactor plan:
- [`agents.TODO.md`](./agents.TODO.md)

## Build, Test, and Development Commands

- `cargo build --release` in `rust-cli/`: build the shipping CLI binary.
- `cargo test --bin agent-bus` in `rust-cli/`: run Rust unit tests.
- `cargo test --test integration_test --test http_integration_test --test channel_integration_test -- --test-threads=1` in `rust-cli/`: run integration tests against local Redis and PostgreSQL.
- `cargo fmt --all --check` and `cargo clippy --all-targets -- -D warnings` in `rust-cli/`: match CI formatting and lint gates.
- `pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease`: repo-root fast iteration build using the shared target-dir, linker, and `sccache` setup.

Set local services with `AGENT_BUS_REDIS_URL` and `AGENT_BUS_DATABASE_URL` when running integration flows.

## Coding Style & Naming Conventions

Use Rust 2024 edition defaults with `rustfmt` width 100 and field init shorthand enabled. Follow Clippy strictly; CI and hooks treat warnings as failures. Use `snake_case` for modules and functions; `CamelCase` only for types. Keep new PowerShell automation in `scripts/` with verb-noun names such as `build-deploy.ps1`.

## Testing Guidelines

Place integration coverage in `rust-cli/tests/*_test.rs`. No fixed coverage percentage is enforced, but every feature change should add or update tests in the affected runtime. Prefer focused unit tests first, then integration coverage for Redis/PostgreSQL behavior, HTTP endpoints, and MCP behavior when transport semantics change.

## Commit & Pull Request Guidelines

Use conventional commits with optional scopes, matching recent history: `feat(http): ...`, `perf(pg): ...`, `docs: ...`, `chore: ...`. Install hooks with `lefthook install`; pre-commit runs `fmt`, `clippy`, and `ast-grep`, and pre-push runs `cargo test`. PRs should describe behavior changes, note required local services or env vars, link issues when applicable, and include screenshots only for dashboard/UI changes.
