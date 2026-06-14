# Agent Bus — Session Context 2026-06-13

- **ID:** `ctx-agent-bus-20260613`
- **Project:** agent-bus (GitHub remote: `David-Martel/agent-hub`)
- **Branch / commit at save:** `main` @ `a151397`
- **Workspace:** `cargo check --workspace` clean (verified 2026-06-13)

## Headline finding: docs lagged the code

A prior session removed the `rust-cli` crate and redistributed its contents into
the four workspace crates. The TODO trackers, `docs/`, and `CLAUDE.md` still
described `rust-cli` as authoritative, so the "structural refactor" backlog was
mostly already **done** while a set of **new breakages** went unrecorded.

### Current real layout
- Workspace members: `crates/agent-bus-core`, `crates/agent-bus-cli`
  (package `agent-bus`, v0.5.0, the `agent-bus` binary), `crates/agent-bus-http`,
  `crates/agent-bus-mcp`.
- Transport files moved into surface crates: `cli.rs`/`commands.rs`/`server_mode.rs`/
  `monitor.rs`/`mcp_discovery.rs` → cli; `http.rs` → http; `mcp.rs` → mcp.
- Shared `McpToolDispatch` lives in `agent-bus-core/src/mcp_dispatch.rs`.
- Integration tests moved to `crates/agent-bus-cli/tests/`:
  `http_integration_test.rs` (47 fns — no longer a skeleton), `channel_integration_test.rs` (6),
  `integration_test.rs` (5).
- `.cargo/config.toml` already workspace-aware (aliases target `-p agent-bus-*`, `lld-link`).

## Open breakages discovered (now tracked in TODO.md P8/P9)

CRITICAL — committed files still point at the removed `rust-cli`:
- `.github/workflows/ci.yml` — 9 `working-directory: rust-cli` refs → CI broken.
- `.github/workflows/release.yml` — 5 `working-directory: rust-cli` refs.
- `build.ps1` — computes `$rustCliDir` and `throw`s "rust-cli directory not found".
- `sgconfig.yml` — ast-grep `languageGlobs` still `rust-cli/src/**/*.rs` (ast-grep finds nothing).

Hygiene / in-flight:
- Uncommitted working-tree migration: modernized `scripts/*.ps1` (already drop
  `rust-cli`), refreshed `examples/mcp/*`, `README.md`/`MCP_CONFIGURATION.md`,
  moved tests, TODO/agents.TODO edits made this session.
- Committed scratch: `errors.json` (~244 KB), `build.log`. Untracked scratch:
  `temp_*.txt`, `build.log.err`, `.cargo/config.override.toml` — none gitignored.
- `agent-bus-cli` still "fat": links `axum`/`rmcp`/`reqwest` because `serve` runs
  HTTP/MCP inline; leftover re-export shim files in `crates/agent-bus-cli/src/`.

Robustness follow-ups (from PR #7/#8 audit):
- Deferred F1: auto-spawn `agent-bus sync` from HTTP bootstrap on PG reconnect
  (currently WARN-only + manual sync). Keep storage layers decoupled.
- No automated regression for `pg_dropped_writes` increment + backfill.
- Audit task-card/presence/subscription/resource-event writes flow through the
  centralized NUL/length guard added at `xadd_to_stream` in PR #8.

## Still genuinely open (pre-existing)
- P4: MCP tool e2e tests (no test exercises stdio/`mcp-http` dispatch); jsonb
  regression coverage; CLI/HTTP parity tests for direct-channel flows.
- P5: qmd operator docs; reproducible bootstrap artifacts.

## Future directions / goals
1. Re-anchor CI/release/build.ps1/sgconfig onto the workspace root (P8 critical).
2. Cross-platform build + validation via system Docker (Linux glibc/musl) —
   local-execution-first.
3. Integrate Rust acceleration/profiling/debug tooling: cargo-nextest, sccache,
   samply/flamegraph, cargo-llvm-cov, cranelift/lld, cargo-flamegraph.
4. Make `agent-bus-cli` a thin surface (resolve `serve` strategy; drop shims).
5. Close P4 test gaps and P9 robustness items.

## Reusable tooling discovered (~/.claude)
- ast-grep Rust rules: `~/.claude/rules/rust/*.yml` + `recommended-cargo-lints.toml`.
- Skills: `cargo-build`, `ci-triage`, `codex-ci-cd`, `rust-new-project`.
- Repo `sgconfig.yml` pulls global rules via `ruleDirs: ~/.claude/rules/rust`.

## Resume hint
Foundational CI/build fixes + the in-flight migration should land first as a
clean base, then the parallel fleet (ci/docs/rust-accel/tests/robustness) merges
on top.

## OUTCOME (end of 2026-06-13 session)
Branch `chore/post-split-remediation` → **PR #11**
(https://github.com/David-Martel/agent-hub/pull/11), 14 commits.

Done this session (fleet of 5 worktree agents, merged + reconciled):
- P8: CI/release/build.ps1/sgconfig re-anchored off `rust-cli`; in-flight
  migration landed; scratch removed+gitignored; docs refreshed; full-tree fmt.
- P9: PG auto-backfill monitor (HTTP bootstrap, decoupled), dropped-writes
  regression test, NUL-guard audit (`reject_nul_in_fields`).
- P4: MCP e2e (7), CLI/HTTP parity (3), jsonb roundtrip (2) — all ran live.
- P10 (new): Docker Linux cross-validate (590 tests pass), profiling profile,
  fixed `.cargo` aliases, `docs/rust-acceleration.md`, install-rust-tooling.{ps1,sh}.

Verification: clippy clean, fmt clean, 749 unit tests, 13 live integration
tests, Linux Docker 590 tests, build.ps1 completes.

**CI caveat (honest):** GitHub `CI` workflow could NOT confirm — the repo has
**no self-hosted runner registered** (ci.yml requires `runs-on: self-hosted`;
last actual CI run was 2026-03-25, all cancelled). Only GitHub-hosted "Copilot"
workflow runs. The workflow *content* is fixed and locally verified, but remote
green is unobtainable until a self-hosted runner is online. Prior PRs #7/#8/#10
merged under the same condition (Copilot-only checks).

STILL OPEN (next session):
- P8: make `agent-bus-cli` a thin surface (resolve `serve` strategy; drop the
  re-export shim modules in `crates/agent-bus-cli/src/`).
- P10: promote Docker CI job to required once a runner is online; optional musl
  release artifact.
- Bring a self-hosted Actions runner online (or switch jobs to GitHub-hosted)
  so CI actually runs.
