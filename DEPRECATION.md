# Python agent-bus-mcp — Deprecated

**Status:** Deprecated as of 2026-03-15. Use the Rust `agent-bus` CLI instead.

## Migration

| Old (Python) | New (Rust) |
|-------------|------------|
| `agent-bus-mcp health` | `agent-bus health` |
| `agent-bus-mcp send ...` | `agent-bus send ...` |
| `agent-bus-mcp read ...` | `agent-bus read ...` |
| `agent-bus-mcp watch ...` | `agent-bus watch ...` |
| `agent-bus-mcp ack ...` | `agent-bus ack ...` |
| `agent-bus-mcp presence ...` | `agent-bus presence ...` |
| `agent-bus-mcp presence-list ...` | `agent-bus presence-list ...` |
| `agent-bus-mcp serve --transport stdio` | `agent-bus serve --transport stdio` |

All flags are identical. The Rust CLI is a drop-in replacement.

## What's new in Rust CLI

- Instant startup (no Python interpreter load)
- Built-in MCP server (`serve --transport stdio`) using rmcp SDK
- `--encoding minimal` for 50% token reduction
- `--encoding human` for terminal-friendly table output
- Full validation (priority enum, non-empty fields, metadata JSON)
- `--version` and comprehensive `--help` with env var docs

## What's kept

The Python package remains at `~/.codex/tools/agent-bus-mcp/` for:
- **PyO3 native codec** used by the MCP server venv (14 Rust functions)
- **PostgreSQL persistence** (not yet ported to Rust CLI)
- **pytest suite** (61 tests) for codec/bus/settings validation
- **PowerShell wrapper scripts** (which can call either CLI)

## MCP configs updated

All three agent platforms now point to the Rust binary:
- `~/.claude/mcp.json` → `~/bin/agent-bus.exe`
- `~/.codex/config.toml` → `~/bin/agent-bus.exe`
- `~/.gemini/settings.json` → `~/bin/agent-bus.exe`

## Timeline

- **2026-03-15:** Python CLI deprecated, Rust CLI is primary
- **Future:** PostgreSQL persistence ported to Rust, Python package archived
