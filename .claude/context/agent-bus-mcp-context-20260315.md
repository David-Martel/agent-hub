# Agent Bus MCP Context — 2026-03-15

## Project
- **Name:** agent-bus-mcp
- **Root:** `~/.codex/tools/agent-bus-mcp`
- **Type:** Rust (MCP server + CLI)
- **Branch:** main @ 3f30007
- **Version:** v0.3.0

## Current State

Rust-based agent coordination bus providing both MCP (stdio) and HTTP (axum) transports.
Features: message send/read/watch/ack, presence tracking, health checks, audio notifications.
PowerShell wrapper scripts provide table formatting and audio notification support.

### Recent Changes (v0.3.0)
- Added axum HTTP REST server transport
- Retired Python CLI, updated PowerShell wrappers to use Rust binary
- Standalone Rust CLI + MCP server (v0.2.0)
- Rust/PyO3 native codec extension
- Comprehensive test suite

### Pending
- MCP startup ordering fix: server init before announcement to avoid MCP handshake interference

## Architecture
```
rust-cli/src/main.rs    — CLI entry, MCP server, HTTP server
scripts/                — PowerShell wrappers (watch, send, read)
tests/                  — Comprehensive test suite
```

## Key Decisions
| Decision | Rationale |
|----------|-----------|
| Rust over Python for CLI | Instant startup, single binary, no runtime deps |
| Dual transport (stdio + HTTP) | MCP compliance + REST accessibility |
| Background announcement | Prevents blocking MCP initialize handshake |

## Agent Registry
| Agent | Task | Status |
|-------|------|--------|
| rust-pro | MCP startup fix | Complete |

## Next Recommendations
1. **test-automator**: Verify MCP handshake timing with the startup fix
2. **security-auditor**: Review HTTP transport for auth/access control
