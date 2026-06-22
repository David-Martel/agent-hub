# Agent Client Installation

This guide covers local agent-bus client installation for Claude, Codex,
Gemini, Antigravity, and adjacent agent launchers.

## Minimum Versions

| Tool | Minimum | Validation |
|------|---------|------------|
| `agent-bus` | `0.5.0` | `agent-bus --version` |
| `agent-bus-mcp` | `0.5.0` | server binary — see note (no `--version`) |
| `agent-bus-http` | `0.5.0` | `curl http://localhost:8400/health` (probes the running service) |
| Rust | `1.85` or newer with edition 2024 support | `rustc --version` |
| PowerShell | `7.0` or newer for scripts | `$PSVersionTable.PSVersion` |
| Redis | `6.2` or newer | `redis-cli INFO server` |
| PostgreSQL | `14` or newer when durable history is enabled | `SHOW server_version;` |

The current workspace version is `0.5.0`. Update all three binaries together;
mixed CLI/MCP/HTTP versions make config validation harder and can hide protocol
drift. **Note:** `agent-bus-http` and `agent-bus-mcp` are server binaries that start
serving on launch, so `--version` does **not** work on them (it just runs the server).
`agent-bus-http` honors only `--port`; `agent-bus-mcp` is stdio-only and takes no flags.
All three are built and deployed at the same workspace version, so check it once with
`agent-bus --version`. To confirm the HTTP service itself is live, probe its endpoint
directly with `curl http://localhost:8400/health`.

## Recommended Defaults

Use explicit loopback IPs for local storage backends:

```powershell
AGENT_BUS_REDIS_URL=redis://127.0.0.1:6380/0
AGENT_BUS_DATABASE_URL=postgresql://postgres@127.0.0.1:5300/redis_backend
```

Use `localhost` or a stable hostname for the agent-facing HTTP URL:

```powershell
AGENT_BUS_SERVER_URL=http://localhost:8400
```

This split avoids Windows `localhost` resolution surprises for Redis/Postgres
while keeping MCP client configuration readable.

## Install

Build and deploy binaries:

```powershell
pwsh -NoLogo -NoProfile -File scripts\setup-agent-hub-local.ps1 -DryRun
pwsh -NoLogo -NoProfile -File scripts\setup-agent-hub-local.ps1
```

Install MCP client entries:

```powershell
pwsh -NoLogo -NoProfile -File scripts\install-mcp-clients.ps1 -DryRun
pwsh -NoLogo -NoProfile -File scripts\install-mcp-clients.ps1
```

Validate without editing live configs:

```powershell
pwsh -NoLogo -NoProfile -File scripts\install-mcp-clients.ps1 -ValidateOnly
pwsh -NoLogo -NoProfile -File scripts\validate-agent-client-configs.ps1
```

The installer prefers `agent-bus-mcp` with no arguments. If only `agent-bus`
is available, it falls back to `agent-bus serve --transport stdio`.

`-DryRun` previews resolved binary paths, backup paths, target client config
files, and environment defaults without writing config, creating directories,
starting services, or touching Cargo caches. Use it before any broad installer
or client-config run on a shared workstation.

## Offline Support Surface

The HTTP service exposes `GET /support` as a static, self-contained operator
page. It requires no internet access and summarizes local health checks,
loopback defaults, IPv4/IPv6 notes, service recovery commands, and bearer-token
requirements for remote binds.

## Multi-Agent Operating Practices

The installer and docs follow the same coordination pattern used by modern
parallel-agent tools such as `rtk-ai/grit`:

- Agents claim work before editing shared files.
- Claims should have TTL/heartbeat renewal for long tasks.
- Contested work should queue or reroute instead of blindly editing.
- Agents work in isolated branches or worktrees when merge risk is high.
- Final merges are serialized by one responsible operator.
- Installer writes are idempotent and backed up before mutation.
- Installer dry-runs should be reviewed before mutation when multiple agents
  are active.
- Config validation runs after install and before merge/deploy.

Agent-bus implements those practices with Redis-backed claims, presence,
direct messages, `request_ack`, and scoped history. Git worktree creation is
still left to the caller; use the bus to advertise ownership and handoffs.

## Client Notes

Claude:
- Preferred config: `~/.claude/mcp.json`
- Preferred transport: stdio
- Preferred command: `agent-bus-mcp`

Codex:
- Preferred config: `~/.codex/config.toml`
- Preferred transport: stdio
- Keep token-bearing HTTP configs out of TOML; use env vars when HTTP is needed.

Gemini:
- Preferred config: `~/.gemini/settings.json`
- The installer writes an `mcpServers.agent-bus` stdio entry.
- Existing HTTP entries using numeric LAN IPs should be replaced with
  `localhost` for same-machine use or a stable hostname for cross-machine use.

Antigravity:
- Validate `~/.antigravity/argv.json` and wrapper commands such as `agy`,
  `antigravity`, and `gemini`.
- Antigravity launchers are treated as client/runtime surfaces; do not store
  agent-bus bearer tokens directly in launcher config.

## Untracked Audit Notes

Machine-local audit files should not be committed verbatim when they contain
live paths, hostnames, IPs, or token placeholders. Promote durable conclusions
into this guide or `MCP_CONFIGURATION.md`, then leave the raw audit local.
