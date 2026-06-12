# Agent-Bus Session Context — 2026-06-12

- **Context ID:** `ctx-agent-bus-20260612-1828`
- **Created:** 2026-06-12T22:28:16Z
- **Created by:** user (Claude Code session)
- **Schema:** v2.0

## Project

| Field | Value |
|-------|-------|
| Name | agent-bus (GitHub: David-Martel/agent-hub) |
| Root | `C:\codedev\agent-bus` |
| Type | rust (Cargo workspace) |
| Branch | `main` |
| Commit | `356acdc` (in sync with `origin/main`) |

## State Summary

This session synchronized the local `agent-bus` repo with its GitHub origin
(`git@github.com:David-Martel/agent-hub.git`). Local `main` was 0 ahead / 7
behind origin; fast-forwarded `427d2ab → 356acdc`, pulling 7 commits. The user
had substantial **uncommitted WIP** (313 insertions across 20 tracked files), so
a **stash → fast-forward pull → stash pop** strategy was used to preserve it.
The pop produced 3 merge conflicts, all resolved in favor of the upstream
versions. `cargo check --workspace --bins` passes clean. **WIP remains
intentionally uncommitted** per the user's earlier choice (sync, don't commit).

## Recent Changes (incoming from origin, now merged)

- `356acdc` build(deps): bump rand in the cargo group (#2)
- `557b0a6` fix(channels): enforce NUL/length validation on direct/group/knock paths (#8)
- `fd67407` fix(robustness): address adversarial audit findings F1-F5 (#7)
- `ddf90c3` chore: fix all clippy -D warnings for clippy 1.96 (181 → 0) (#6)
- `c1c909b` feat(cli): send bearer token on server-mode HTTP requests (#5)
- `b65b895` feat(http,core): optional bearer-token auth + gated cross-machine bind (#4)
- `9c4cddb` fix(cli,core): health exits non-zero on unhealthy bus; batch errors name the item (#3)

## Work In Progress (uncommitted, preserved via stash/pop)

26 working-tree entries: 20 modified tracked files + 6 untracked.

Modified (selected): `MCP_CONFIGURATION.md`, `README.md`, `TODO.md`,
`agents.TODO.md`, `crates/agent-bus-cli/src/server_mode.rs`,
`crates/agent-bus-cli/tests/http_integration_test.rs`,
`crates/agent-bus-cli/tests/integration_test.rs`,
`crates/agent-bus-core/src/ops/message.rs`,
`crates/agent-bus-core/src/settings.rs`, `lefthook.yml`, `scripts/*.ps1`,
`examples/mcp/*`.

Untracked junk (candidates for cleanup / `.gitignore`):
`temp_dashboard.txt`, `temp_filters.txt`, `temp_filters2.txt`,
`temp_test_out.txt`, `build.log.err`, `.cargo/config.override.toml`.

## Blockers

None. Workspace compiles. WIP intentionally uncommitted.

## Decisions

### dec-001 — Sync strategy: stash, pull, restore
- **Decision:** Use `git stash -u` → `git pull --ff-only` → `git stash pop` to
  bring in origin's 7 commits while preserving uncommitted WIP.
- **Rationale:** User had ~313 lines of uncommitted WIP overlapping 5 incoming
  files; this preserves it without forcing a commit and is fully reversible.
- **Alternatives:** (a) Commit WIP then pull+push; (b) Pull only, user handles WIP.
- **Decided by:** user (AskUserQuestion).

### dec-002 — Conflict resolution favored upstream in all 3 files
- **Decision:** For `server_mode.rs`, `ops/message.rs`, `settings.rs`, take the
  upstream side of each conflict.
- **Rationale:** `server_mode.rs` / `settings.rs` conflicts were cosmetic
  (Unicode `→` vs ASCII `->`, fuller doc comments). `ops/message.rs` required
  upstream's `at` error-tagging closure — the stashed `_ctx` variant referenced
  no longer-existing code and would not compile.
- **Decided by:** Claude (verified by `cargo check --workspace --bins`).

## Patterns / Conventions Observed

- Build via Bash tool, not PowerShell: the PS profile mangles `$`-prefixed
  variables (every `$var` errors as an unknown command) and `cargo` resolves to
  a `.ps1` shim with ambiguous `-p` parsing. Use `RUSTC_WRAPPER="" cargo ...`
  from Bash to bypass sccache wrapper for debugging.
- Edition 2024, mimalloc, `anyhow::Result` + `.context()`, clippy pedantic +
  restriction subset (workspace lints). clippy 1.96 clean as of `ddf90c3`.

## Agent Work Registry

| Agent | Task | Files | Status | Handoff |
|-------|------|-------|--------|---------|
| (main) | Repo sync + conflict resolution | server_mode.rs, ops/message.rs, settings.rs | Complete | WIP uncommitted; compiles clean |

## Recommended Next Steps

1. **User decision on WIP** — commit the 20-file WIP in logical clusters
   (`commit-cluster`), or continue editing. It is currently uncommitted by design.
2. **Cleanup** — remove or `.gitignore` the 6 untracked junk files
   (`temp_*.txt`, `build.log.err`, `.cargo/config.override.toml`).
3. **rust-pro / cargo-build** — if resuming WIP, run full
   `cargo test --workspace` (needs Redis :6380 + PG :5300 for integration tests).

## Roadmap Pointers

- Active roadmap: `TODO.md` (P0–P6), structural refactor in `agents.TODO.md`.
- Phase 3 crate split: `docs/phase3-crate-split-plan-2026-04-04.md`.
