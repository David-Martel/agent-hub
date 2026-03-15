# Agent Bus MCP

Rust-native coordination bus for Codex, Claude, Gemini, and local sub-agents.

The active implementation lives in `rust-cli/` and uses Redis for live
transport plus PostgreSQL for durable history and presence-event persistence.
The legacy Python package remains in the repo only as deprecated reference
material.

Current protocol metadata:
- Bus protocol: `agent-bus` message contract v1.0
- Message identity: UUID `id` + UTC `timestamp_utc`
- Default Redis endpoint: `redis://localhost:6380/0`
- Default PostgreSQL endpoint: `postgresql://postgres@localhost:5432/redis_backend`

## Commands

```powershell
pwsh -NoLogo -NoProfile -File C:\Users\david\.codex\tools\agent-bus-mcp\scripts\send-agent-bus.ps1 -From codex -To claude -Topic status -Body "message" -Encoding compact
pwsh -NoLogo -NoProfile -File C:\Users\david\.codex\tools\agent-bus-mcp\scripts\read-agent-bus.ps1 -Agent codex -SinceMinutes 120
pwsh -NoLogo -NoProfile -File C:\Users\david\.codex\tools\agent-bus-mcp\scripts\watch-agent-bus.ps1 -Agent codex
uv --directory C:\Users\david\.codex\tools\agent-bus-mcp run agent-bus-mcp health
pwsh -NoLogo -NoProfile -File C:\Users\david\.codex\tools\agent-bus-mcp\scripts\validate-agent-bus.ps1
```

### Windows service

```powershell
# Install the service (requires admin + nssm in PATH)
pwsh -NoLogo -NoProfile -File scripts\install-agent-hub-service.ps1

# Configure log rotation (10 MB max, daily rotation)
pwsh -NoLogo -NoProfile -File scripts\configure-log-rotation.ps1

# Remove the service
pwsh -NoLogo -NoProfile -File scripts\remove-agent-hub-service.ps1
```

#### Service troubleshooting

```powershell
# Check service status
nssm status AgentHub

# Restart the service
nssm restart AgentHub

# View recent logs
Get-Content C:\ProgramData\AgentHub\logs\agent-hub-service.log -Tail 50

# View error logs
Get-Content C:\ProgramData\AgentHub\logs\agent-hub-service-error.log -Tail 50
```

Common issues:
- **Port in use**: Another process is using port 8400. Check with `netstat -ano | findstr 8400`.
- **Redis not running**: Verify with `redis-cli -p 6380 ping`. Start the Redis service if needed.
- **PostgreSQL connection refused**: Verify with `psql -h localhost -U postgres -c "SELECT 1"`.
- **Service won't start**: Check error log, ensure `agent-bus.exe` exists at `C:\Users\david\bin\agent-bus.exe`.

### MCP launch

```powershell
uv --directory C:\Users\david\.codex\tools\agent-bus-mcp run agent-bus-mcp serve --transport stdio
uv --directory C:\Users\david\.codex\tools\agent-bus-mcp run agent-bus-mcp serve --transport streamable-http --port 8765
```

## Notes

- Live message durability and fanout use Redis Streams + Pub/Sub.
- Durable history and presence events are mirrored into PostgreSQL when configured.
- Realtime watch uses Redis Pub/Sub.
- PowerShell wrapper scripts call the Rust binary directly and default to local `localhost` Redis/PostgreSQL services.
- `scripts/install-agent-hub-service.ps1` installs the native HTTP transport as a Windows service using `nssm` + `pwsh`.
- MCP clients should register the stdio form by default.
- Streamable HTTP is available for clients that support long-lived MCP sessions and notifications.
- MCP server validates Redis and PostgreSQL during startup and emits a startup status message from `agent-bus`.
- Redis remains the realtime system of record; PostgreSQL is the query/history backend.
- Sample client configs are in `examples/mcp/`.
- Agent coordination guidance is in `AGENT_COMMUNICATIONS.md` and `MCP_CONFIGURATION.md`.

## Encoding and interoperability

- Default CLI output remains human-readable JSON; prefer `--encoding compact` for LLM prompts and scripts.
- `thread_id` can be used for grouped coordination conversations.
- `protocol_version` is added to all outbound payloads for safe schema evolution.
- Planned future: adapter mode for A2A task cards and protobuf/TOON payload negotiation for selected endpoints.

### Interop & runtime metadata

- `bus_health` now returns `protocol_version` and `runtime_metadata` for monitoring hooks.
- `bus_health.runtime_metadata.codec` reports serializer backend (`native`/`python`) and is safe for observability pipelines.
- Suggested integration order:
  - Use compact JSON for all machine pipelines and script-to-script transport.
  - Use human mode only for direct operator viewing.
  - Keep payload contracts additive (`protocol_version`, `thread_id`) to preserve forward compatibility.
- See [IMPLEMENTATION_NOTES.md](./IMPLEMENTATION_NOTES.md) for a concrete A2A/TOON/protobuf comparison and Rust migration sequence.

### Performance and Rust path

- High-throughput target: keep Redis stream reads/writes in Python for now, then migrate hot serialization/deserialization paths (`message` / `event` shaping + watch formatting) to a Rust `pyo3` extension with:
  - zero-copy JSON serialization for NDJSON/compact output,
  - bounded message reuse in worker pools, and
  - optional C ABI fallback while retaining pure-Python path parity.
