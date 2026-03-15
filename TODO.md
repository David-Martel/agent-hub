# Agent Hub — TODO

## Completed

- [x] Remove `agent-bus-mcp` uv tool installation
- [x] Update all MCP configs to use Rust binary (Claude, Codex, Gemini)
- [x] Remove Python CLI entry point from pyproject.toml
- [x] Update PowerShell scripts to call Rust binary
- [x] HTTP server mode (`serve --transport http --port 8400`)
- [x] REST API: GET /health, POST /messages, GET /messages, POST /messages/:id/ack
- [x] PUT /presence/:agent, GET /presence
- [x] Connection pool pattern (new connection per request, ready for pooling)

## Remaining

### Architecture (P3)
- [ ] Extract main.rs into modules: settings, models, bus, output, cli, mcp, http, tests
- [ ] Redis connection pooling (reuse across requests in HTTP mode)
- [ ] `spawn_blocking` for sync Redis in async handlers

### Wire Formats (P4)
- [ ] MessagePack encoding for Redis stream storage (`--wire msgpack`)
- [ ] LZ4 compression for message bodies > 256 bytes
- [ ] URL credential redaction in health output

### Advanced (P5)
- [ ] SSE streaming endpoint: GET /messages/stream?agent=X
- [ ] `--server` CLI flag for HTTP client mode
- [ ] Async Tokio Redis client
- [ ] SIMD JSON parsing
- [ ] Windows Service via NSSM
- [ ] A2A protocol adapter
- [ ] PostgreSQL persistence (port from Python)
