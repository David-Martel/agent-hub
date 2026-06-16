# Agent-Bus Configuration Alignment Audit
**Date:** 2026-06-14
**Auditor:** Claude Code (automated read-only audit, no services restarted, no config files edited)
**Scope:** Drift between the source-of-truth Rust codebase and every installed client/service reference on this machine.
---
## 1. Discrepancy Table
Legend: DRIFT? column — **YES** = value diverges from canonical or is unsafe; **OK** = aligned; **NOTE** = informatio...
| Setting | Source-of-Truth Value | `~/.config/agent-bus/config.json` | `~/.claude/mcp.json` | `~/.codex/config.toml`...
|---|---|---|---|---|---|---|---|---|---|---|
| **Redis URL** (`AGENT_BUS_REDIS_URL`) | `redis://localhost:6380/0` | *(absent — config.json only sets server_url + ...
| **PostgreSQL URL** (`AGENT_BUS_DATABASE_URL`) | `postgresql://postgres@localhost:5300/redis_backend` | *(absent)* |...
| **Redis bind family** | Redis process listens on `127.0.0.1:6380` (IPv4 only). Client URL uses `localhost`, which r...
| **PostgreSQL bind family** | `postgres` listens on both `::1:5300` and `127.0.0.1:5300`. Client URL uses `localhost...
| **HTTP server host** (`AGENT_BUS_SERVER_HOST`) | Default: `localhost` (loopback-only bind). Off-localhost requires ...
| **HTTP server bind address (live process)** | `agent-bus-http` is running on `::1:8400` (IPv6 loopback only per dia...
| **HTTP server URL (client-side)** | Canonical: `http://localhost:8400` (per `MCP_CONFIGURATION.md`, `AGENT_COORDINA...
| **MCP transport** (`~/.claude/mcp.json`) | Canonical intent: **stdio** for local MCP clients (per `MCP_CONFIGURATIO...
| **Auth token** (`AGENT_BUS_AUTH_TOKEN`) | Required when `AGENT_BUS_ALLOW_REMOTE=true` or `AGENT_BUS_SERVER_HOST != ...
| **`AGENT_BUS_ALLOW_REMOTE`** | `false` by default; must be `true` to bind off-localhost | *(absent in config.json —...
| **`agent-bus.exe` in `~/bin/`** | Should be present (per `MCP_CONFIGURATION.md`: "AgentHub should run from `%USERPR...
| **`AGENT_BUS_STARTUP_ENABLED`** | Default `true` in config template | Examples and installed configs set `"false"` ...
| **`rag-redis` Redis port in `~/.claude/mcp.json`** | Agent-bus uses `6380`; rag-redis also uses `redis://localhost:...
| **`unified-orchestrator` Redis port** | Agent-bus Redis: `6380` | — | `"REDIS_HOST": "localhost", "REDIS_PORT": "63...
| **Git remote** | `origin` → `git@github.com:David-Martel/agent-hub.git` | — | — | — | — | — | — | — | NOTE | Inform...
---
## 2. Canonical Aligned Configuration
### 2a. Design intent: external-computer-facing
The auth token in `~/.config/agent-bus/config.json`, `~/.claude/mcp.json`, and `~/.gemini/settings.json`, and the har...
**Server (`agent-bus-http.exe` environment / NSSM service configuration):**
```json
{
  "server_host":    "0.0.0.0",
  "redis_url":      "redis://127.0.0.1:6380/0",
  "database_url":   "postgresql://postgres@127.0.0.1:5300/redis_backend",
  "machine_safe":   false
}
```
Plus these environment variables injected into the service process (not stored in config.json):
```
AGENT_BUS_ALLOW_REMOTE=true
AGENT_BUS_AUTH_TOKEN=<token>          # currently <REDACTED-TOKEN>... — read from vault/env, never hard-coded in JSON
AGENT_BUS_HTTP_PORT=8400
```
**Why `0.0.0.0` and not `192.168.50.79`:** The Rust code constructs the bind address as `format!("{bind_host}:{port}"...
**Why `127.0.0.1` for Redis and PostgreSQL, not `localhost`:** The Redis bind confirmed as `127.0.0.1:6380` (IPv4 onl...
**Client-side:** Every MCP client should reference the bus via a hostname or env var, not a numeric IP:
```
AGENT_BUS_SERVER_URL=http://<HOSTNAME>:8400
# or for local-only use:
AGENT_BUS_SERVER_URL=http://localhost:8400
```
Where `<HOSTNAME>` is the stable hostname of this machine (e.g. `asuspro13` or its FQDN). This survives DHCP changes ...
---
### 2b. IPv4/IPv6 fix for Redis
**Root cause:** Redis binds `127.0.0.1:6380` (IPv4 only). Windows DNS resolves `localhost` to `::1` (IPv6) first. The...
**Fix options (in order of preference):**
1. **[RECOMMENDED] Change Redis client URL to `redis://127.0.0.1:6380/0`** — set in `server_host`'s backing config.js...
2. **Configure Redis to also bind `::1`** — add `bind 127.0.0.1 ::1` to `redis.conf`. Allows keeping `redis://localho...
3. **Fix Windows DNS localhost resolution** — edit `C:\Windows\System32\drivers\etc\hosts` to put `127.0.0.1 localhos...
Option 1 is the safest and most targeted change.
---
## 3. Per-File Proposed Change List (safest first)
Changes are ordered: lowest blast radius first. Items marked DO-NOT-APPLY are for reference only per the audit constr...
---
### Change 1 — `~/.config/agent-bus/config.json` (user-owned runtime config — safe to apply)
**Current:**
```json
{
  "server_url": "http://localhost:8400",
  "auth_token": "<REDACTED-TOKEN>"
}
```
**Proposed:**
```json
{
  "server_url": "http://localhost:8400",
  "auth_token": "<redacted — keep as is>"
}
```
No content change needed here; this file is correct for local client use. However, also add:
```json
{
  "server_url":  "http://localhost:8400",
  "redis_url":   "redis://127.0.0.1:6380/0",
  "database_url": "postgresql://postgres@127.0.0.1:5300/redis_backend",
  "auth_token":  "<redacted>"
}
```
**Rationale:** Pinning `redis_url` and `database_url` to `127.0.0.1` in the config file prevents any future env-var a...
---
### Change 2 — `C:/codedev/agent-bus/MCP_CONFIGURATION.md` (source doc — can update freely)
The doc still contains the stale note: *"The repo has a workspace and dedicated wrapper crates, but the MCP surface i...
**Proposed addition to "Environment defaults" block:**
```
- `AGENT_BUS_ALLOW_REMOTE=true`   (required for off-localhost bind)
- `AGENT_BUS_AUTH_TOKEN=<token>`  (required when ALLOW_REMOTE=true; set via NSSM env, not config.json)
- `AGENT_BUS_HTTP_PORT=8400`      (explicit; matches DEFAULT_PORT)
- `AGENT_BUS_REDIS_URL=redis://127.0.0.1:6380/0`   (IPv4-explicit to avoid localhost→::1 mismatch on Windows)
- `AGENT_BUS_DATABASE_URL=postgresql://postgres@127.0.0.1:5300/redis_backend`
```
**Remove stale rust-cli references.**
---
### Change 3 — NSSM Windows Service environment (apply via NSSM or service restart — medium blast radius)
The `AgentHub` Windows service (`agent-bus-http.exe`) needs these env vars injected into its service process to enabl...
```powershell
nssm set AgentHub AppEnvironmentExtra `
  "AGENT_BUS_SERVER_HOST=0.0.0.0" `
  "AGENT_BUS_ALLOW_REMOTE=true" `
  "AGENT_BUS_AUTH_TOKEN=<redacted>" `
  "AGENT_BUS_REDIS_URL=redis://127.0.0.1:6380/0" `
  "AGENT_BUS_DATABASE_URL=postgresql://postgres@127.0.0.1:5300/redis_backend" `
  "AGENT_BUS_HTTP_PORT=8400"
```
Then restart: `agent-bus service --action restart --reason "apply external bind config"`
**Rationale:** Without `AGENT_BUS_ALLOW_REMOTE=true`, the settings validator rejects a non-localhost `server_host`, c...
---
### Change 4 — `~/.codex/config.toml` [agent_bus] section (PROPOSED DIFF ONLY — do not apply)
**Current:**
```toml
[mcp_servers.agent_bus]
url = "http://localhost:8400/mcp"
bearer_token_env_var = "AGENT_BUS_AUTH_TOKEN"
startup_timeout_sec = 30
tool_timeout_sec = 60
```
**Proposed diff:** No change needed for local use. This is correct. For cross-machine access, change only the `url`:
```toml
[mcp_servers.agent_bus]
url = "http://asuspro13:8400/mcp"    # or use AGENT_BUS_SERVER_URL env var
bearer_token_env_var = "AGENT_BUS_AUTH_TOKEN"
startup_timeout_sec = 30
tool_timeout_sec = 60
```
**Rationale:** `localhost:8400` is correct when Codex runs on the same machine. For cross-machine Codex, replace with...
---
### Change 5 — `~/.claude/mcp.json` [agent-bus] entry (PROPOSED DIFF ONLY — DO NOT APPLY — live file)
**Current:**
```json
"agent-bus": {
  "type": "http",
  "url": "http://192.168.50.79:8400/mcp",
  "headers": { "Authorization": "Bearer <REDACTED-TOKEN>..." }
}
```
**Proposed replacement:**
```json
"agent-bus": {
  "type": "stdio",
  "command": "C:\\Users\\david\\bin\\agent-bus-mcp.exe",
  "env": {
    "AGENT_BUS_REDIS_URL":      "redis://127.0.0.1:6380/0",
    "AGENT_BUS_DATABASE_URL":   "postgresql://postgres@127.0.0.1:5300/redis_backend",
    "AGENT_BUS_SERVER_HOST":    "localhost",
    "AGENT_BUS_SERVER_URL":     "http://localhost:8400",
    "AGENT_BUS_STARTUP_ENABLED": "false",
    "AGENT_BUS_AUTH_TOKEN":     "${env:AGENT_BUS_AUTH_TOKEN}",
    "RUST_LOG":                 "error"
  }
}
```
**Rationale (three sub-reasons):**
1. **Eliminate hard-coded IP** — `192.168.50.79` violates the CLAUDE.md rule ("never numeric loopback/external IP") a...
2. **Prefer stdio over HTTP** — stdio MCP is the canonical recommendation in `MCP_CONFIGURATION.md` for Claude Code/D...
3. **Eliminate hard-coded auth token** — token should come from the environment (`${env:AGENT_BUS_AUTH_TOKEN}`) not b...
If HTTP transport must be kept (e.g., Claude is running on a different machine), change only the `url`:
```json
"url": "http://asuspro13:8400/mcp"
```
---
### Change 6 — `~/.gemini/settings.json` [agent-bus] entry (PROPOSED DIFF ONLY — DO NOT APPLY — live file)
**Current:**
```json
"agent-bus": {
  "httpUrl": "http://192.168.50.79:8400/mcp",
  "headers": { "Authorization": "Bearer <REDACTED-TOKEN>..." }
}
```
**Proposed:**
```json
"agent-bus": {
  "httpUrl": "http://asuspro13:8400/mcp",
  "headers": { "Authorization": "Bearer ${AGENT_BUS_AUTH_TOKEN}" }
}
```
(Or replace with stdio if Gemini CLI supports it — check Gemini MCP client docs.)
**Rationale:** Same as Change 5 — eliminate hard-coded IP and hard-coded token. Use stable hostname. The `SessionStar...
---
### Change 7 — `C:/codedev/agent-bus/examples/mcp/gemini.settings.json` (example doc)
Update to use hostname placeholder instead of `localhost` to set a better example for cross-machine setups:
```json
"AGENT_BUS_SERVER_URL": "http://${AGENT_BUS_HOST:-localhost}:8400"
```
Note the example files currently show `localhost` which is fine for single-machine use. Document the `AGENT_BUS_HOST`...
---
### Change 8 — `C:/codedev/agent-bus/examples/mcp/codex.config.toml` (example doc)
Add a comment explaining the cross-machine URL pattern:
```toml
# For cross-machine access: set AGENT_BUS_SERVER_URL=http://<hostname>:8400 in the environment
# and set AGENT_BUS_AUTH_TOKEN to the bearer token.
[mcp_servers.agent_bus]
... (32 lines truncated)
