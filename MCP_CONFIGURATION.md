# MCP Configuration

## Local runtime

Current code-grounded status:

- The repo is a four-crate Cargo workspace; the old `rust-cli` crate has been
  removed.
- `agent-bus-mcp.exe` is the dedicated stdio MCP server and delegates tool
  execution through `agent-bus-core`.
- The build, validate, and deploy scripts compile from the workspace root and
  deploy `agent-bus.exe`, `agent-bus-http.exe`, and `agent-bus-mcp.exe`.
- See [`docs/current-status-2026-06-13.md`](./docs/current-status-2026-06-13.md)
  for the broader architecture/status checkpoint.

Use the Rust-native MCP server through stdio. Prefer the purpose-built
wrapper binary, which takes no arguments and skips the full CLI clap
parser on startup:

`agent-bus-mcp.exe`

The monolithic CLI path remains functionally equivalent as a fallback:

`agent-bus.exe serve --transport stdio`

Environment defaults on this machine:

- `AGENT_BUS_REDIS_URL=redis://127.0.0.1:6380/0`
- `AGENT_BUS_DATABASE_URL=postgresql://postgres@127.0.0.1:5300/redis_backend`
- `AGENT_BUS_SERVER_HOST=localhost`
- `AGENT_BUS_SERVER_URL=http://localhost:8400`
- `AGENT_BUS_MACHINE_SAFE=false`

Use `127.0.0.1` for local Redis/PostgreSQL URLs on Windows unless you have
confirmed those services bind both `::1` and `127.0.0.1`. The HTTP service URL
can stay `http://localhost:8400` for same-machine clients.

If Redis is unavailable on a fresh machine, use the maintained
[redis-windows](https://github.com/redis-windows/redis-windows) repository.
On the primary Windows dev machine this repo may already exist as a local
checkout under `T:\projects\redis-windows`.

## Important constraint

Binary selection by use case:

- **Stdio MCP clients (Claude Code, Claude Desktop, Codex, Cursor, Gemini CLI,
  VS Code)**: use `agent-bus-mcp.exe` with no arguments. This is the thin
  wrapper binary whose `main.rs` wires stdin/stdout straight into
  `rmcp::serve_server` without parsing any CLI arguments, so the clap
  subcommand tree of `agent-bus.exe` is not loaded on startup.
- **Fallback stdio MCP**: `agent-bus.exe serve --transport stdio`. Functionally
  equivalent to `agent-bus-mcp.exe` — pick this only if `agent-bus-mcp.exe` is
  not present in `~/bin` yet (e.g. on a machine built before
  `scripts/build-deploy.ps1` added the third binary).
- **Backend health checks and local transport debugging**: `agent-bus.exe`
  (the monolithic CLI with `health`, `send`, `read`, `watch`, `monitor`,
  `service`, etc.).
- **Long-running local HTTP/SSE service on `http://localhost:8400`**:
  `agent-bus-http.exe`. This is the binary the NSSM-managed `AgentHub`
  Windows service points at; it ignores argv and always listens on the
  compiled-in `DEFAULT_PORT`.
- **Local Streamable HTTP MCP surface**:
  `agent-bus.exe serve --transport mcp-http` (default port 8401).

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

Validate installed client entries without editing them with:

`pwsh -NoLogo -NoProfile -File scripts\validate-agent-client-configs.ps1`

See [docs/agent-client-installation.md](./docs/agent-client-installation.md)
for version minimums, install/validate workflow, and multi-agent operating
practices.

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
