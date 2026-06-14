# Public Release Checklist

Use this before opening the repository beyond the current machine.

Current status reference:
- [`current-status-2026-06-13.md`](./current-status-2026-06-13.md)

If the repo is made public before the remaining operator hardening work is
complete, make sure the public docs explicitly say so. The old `rust-cli` crate
has been removed; current public docs should describe the four-crate workspace,
the independent `agent-bus`, `agent-bus-http`, and `agent-bus-mcp` binaries, and
the remaining backlog in `TODO.md` / `agents.TODO.md`.

## Keep In Repo

- Source code in `crates/`, `scripts/`, `examples/`, `docs/`, and `web/`
- Generic MCP examples that use `agent-bus.exe` on `PATH`
- Localhost-only service defaults:
  - `redis://127.0.0.1:6380/0`
  - `postgresql://postgres@127.0.0.1:5300/redis_backend`
- Redis fallback guidance that points to the maintained
  [redis-windows](https://github.com/redis-windows/redis-windows) repository
- Agent protocol docs and design notes that do not expose personal state

## Keep Out Of Repo

- Repo-local agent state such as `/.claude/`, `/.codex/`, `/.serena/`
- Generated logs, scratch builds, SQLite state, and temporary files
- User-specific home paths beyond docs that explicitly describe Windows install locations
- Secrets, tokens, API keys, or credential-bearing URLs

## Publish Checks

1. Run `rg -n "password|secret|token|api[_-]?key|C:\\\\Users\\\\|postgresql://|redis://" . -g '!target' -g '!**/target/**'`.
1. Confirm `.gitignore` excludes local state, logs, and scratch build output.
1. Confirm `.rgignore` only re-exposes the specific generated logs you want LLM agents to inspect without reintroducing them to git.
1. Confirm README, MCP docs, and the current-status note describe the real current architecture, including remaining wrapper surfaces or operator hardening work.
1. Run `pwsh -NoLogo -NoProfile -File scripts\validate-agent-bus.ps1 -DryRun`, then `pwsh -NoLogo -NoProfile -File scripts\validate-agent-bus.ps1 -SkipBuild -SkipTests`.
1. Verify examples under `examples/mcp/` use generic commands and localhost-only defaults.
1. Confirm repository visibility is intentional (`gh repo view --json visibility,isPrivate`) and consistent with the release state you are documenting.

## Client Install Flow

- Local binaries: `pwsh -NoLogo -NoProfile -File scripts\setup-agent-hub-local.ps1 -DryRun`
- Windows service: `pwsh -NoLogo -NoProfile -File scripts\install-agent-hub-service.ps1 -DryRun`
- Claude + Codex + Gemini MCP config: `pwsh -NoLogo -NoProfile -File scripts\install-mcp-clients.ps1 -DryRun`
- Redis install fallback: `https://github.com/redis-windows/redis-windows`

Run the same commands without `-DryRun` only after the previewed paths,
environment values, and target clients are correct.
