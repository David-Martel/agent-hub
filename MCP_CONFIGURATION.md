# MCP Configuration

## Local runtime

Current code-grounded status:

- The repo has a workspace and dedicated wrapper crates, but the MCP surface is
  still operationally rooted in `rust-cli`.
- `agent-bus-mcp.exe` is a thin wrapper over the shared `rust-cli` runtime.
- The build, validate, and deploy scripts still compile and discover binaries
  from the `rust-cli` build tree.
- See [`docs/current-status-2026-04-03.md`](./docs/current-status-2026-04-03.md)
  for the broader architecture/status checkpoint.

Use the Rust-native MCP server through stdio:

`agent-bus.exe serve --transport stdio`

Environment defaults on this machine:

- `AGENT_BUS_REDIS_URL=redis://localhost:6380/0`
- `AGENT_BUS_DATABASE_URL=postgresql://postgres@localhost:5300/redis_backend`
- `AGENT_BUS_SERVER_HOST=localhost`
- `AGENT_BUS_MACHINE_SAFE=false`

If Redis is unavailable on a fresh machine, use the maintained
[redis-windows](https://github.com/redis-windows/redis-windows) repository.
On the primary Windows dev machine this repo may already exist as a local
checkout under `T:\projects\redis-windows`.

## Important constraint

Use `agent-bus-mcp.exe` or `agent-bus.exe serve --transport stdio` for stdio
MCP clients. Use `agent-bus.exe` for backend health checks and local transport
debugging. Use `agent-bus-http.exe` for the long-running local HTTP/SSE
service on `http://localhost:8400`. Use `agent-bus.exe serve --transport mcp-http`
for the local Streamable HTTP MCP surface when the client supports it.

If a scripted workflow is capturing `compact`, `json`, `minimal`, or `toon`
output for another tool, set `AGENT_BUS_MACHINE_SAFE=true` to suppress
non-fatal degraded PostgreSQL fallback warnings from mixing into the capture.

For local build/test/deploy work on a busy multi-repo machine, prefer private
cargo artifact namespaces:

- `pwsh -NoLogo -NoProfile -File build.ps1 -Release -TargetNamespace codex-local`
- `pwsh -NoLogo -NoProfile -File scripts\build-deploy.ps1 -TargetDir T:\RustCache\cargo-target\codex-http-deploy`

It does expose local MCP Streamable HTTP via `serve --transport mcp-http`, but
it is still packaged as a localhost/self-hosted surface. If you want to expose
it beyond the local machine, add your own auth, proxying, and process
management rather than treating the current local HTTP service as a public MCP
endpoint.

## Sample files

Sample configurations live under `examples/mcp/`:

- `codex.config.toml`
- `claude-desktop.config.json`
- `gemini.settings.json`
- `cursor.mcp.json`
- `vscode.mcp.json`
- `generic-stdio.mcp.json`

Install local client entries with:

`pwsh -NoLogo -NoProfile -File scripts\install-mcp-clients.ps1`

## Windows locations

- Codex: `%USERPROFILE%\.codex\config.toml`
- Claude Desktop: `%APPDATA%\Claude\claude_desktop_config.json`
- Gemini CLI: `%USERPROFILE%\.gemini\settings.json`
- Cursor: `%USERPROFILE%\.cursor\mcp.json`
- VS Code project config: `<repo>\.vscode\mcp.json`

## Recommended server names

Use `agent-bus` in JSON configs and `agent_bus` in TOML configs. Keep the name
stable once a client is configured so prompts and habits do not drift.

## First commands after connecting

1. `bus_health`
2. `set_presence`
3. `list_presence`
4. `list_messages`

## Prompt snippet

Add this behavior to your client-specific system/project instructions:

`Use the agent-bus MCP server for agent coordination, handoffs, presence, and inbox history before making assumptions about parallel work.`
