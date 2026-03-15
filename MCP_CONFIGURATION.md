# MCP Configuration

## Local runtime

Use the Rust-native MCP server through stdio:

`C:\Users\david\bin\agent-bus.exe serve --transport stdio`

Environment defaults on this machine:

- `AGENT_BUS_REDIS_URL=redis://localhost:6380/0`
- `AGENT_BUS_DATABASE_URL=postgresql://postgres@localhost:5432/redis_backend`
- `AGENT_BUS_SERVER_HOST=localhost`

## Important constraint

The local Agent Hub service exposes:

- MCP over stdio
- REST over `http://localhost:8400`

It does not currently expose remote MCP over streaming HTTP or SSE. Remote-only
clients such as ChatGPT developer mode need a future remote MCP wrapper rather
than the current local HTTP service.

## Sample files

Sample configurations live under `examples/mcp/`:

- `codex.config.toml`
- `claude-desktop.config.json`
- `gemini.settings.json`
- `cursor.mcp.json`
- `vscode.mcp.json`
- `generic-stdio.mcp.json`

## Windows locations

- Codex: `%USERPROFILE%\.codex\config.toml`
- Claude Desktop: `%APPDATA%\Claude\claude_desktop_config.json`
- Gemini CLI: `%USERPROFILE%\.gemini\settings.json`
- Cursor: `%USERPROFILE%\.cursor\mcp.json`
- VS Code project config: `<repo>\.vscode\mcp.json`

## Recommended server name

Use `agentHub` as the MCP server name across clients. Short and stable names
help tool selection.

## First commands after connecting

1. `bus_health`
2. `set_presence`
3. `list_presence`
4. `list_messages`

## Prompt snippet

Add this behavior to your client-specific system/project instructions:

`Use the agentHub MCP server for agent coordination, handoffs, presence, and inbox history before making assumptions about parallel work.`
