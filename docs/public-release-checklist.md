# Public Release Checklist

Use this before opening the repository beyond the current machine.

## Keep In Repo

- Source code in `rust-cli/`, `scripts/`, `examples/`, and `docs/`
- Generic MCP examples that use `agent-bus.exe` on `PATH`
- Localhost-only service defaults:
  - `redis://localhost:6380/0`
  - `postgresql://postgres@localhost:5300/redis_backend`
- Redis fallback guidance that points to the maintained
  [redis-windows](https://github.com/redis-windows/redis-windows) repository
- Agent protocol docs and design notes that do not expose personal state

## Keep Out Of Repo

- Repo-local agent state such as `/.claude/`, `/.codex/`, `/.serena/`
- Generated logs, scratch builds, SQLite state, and temporary files
- User-specific home paths beyond docs that explicitly describe Windows install locations
- Secrets, tokens, API keys, or credential-bearing URLs

## Publish Checks

1. Run `rg -n "password|secret|token|api[_-]?key|C:\\\\Users\\\\|postgresql://|redis://" . -g '!rust-cli/target'`.
1. Confirm `.gitignore` excludes local state, logs, and scratch build output.
1. Confirm `.rgignore` only re-exposes the specific generated logs you want LLM agents to inspect without reintroducing them to git.
1. Confirm README and MCP docs mention the `agent-bus.exe` / `agent-bus-http.exe` split and a Redis fallback install path.
1. Run `pwsh -NoLogo -NoProfile -File scripts\validate-agent-bus.ps1 -SkipBuild -SkipTests`.
1. Verify examples under `examples/mcp/` use generic commands and localhost-only defaults.

## Client Install Flow

- Local binaries: `pwsh -NoLogo -NoProfile -File scripts\setup-agent-hub-local.ps1`
- Windows service: `pwsh -NoLogo -NoProfile -File scripts\install-agent-hub-service.ps1`
- Claude + Codex MCP config: `pwsh -NoLogo -NoProfile -File scripts\install-mcp-clients.ps1`
- Redis install fallback: `https://github.com/redis-windows/redis-windows`
