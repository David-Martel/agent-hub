# Agent Bus Reliability Plan - 2026-06-26

## Findings

- The AgentHub HTTP service is installed with bearer authentication enabled,
  but the same-machine client config did not contain `auth_token`. `/health`
  worked while `/messages` and `/presence` returned 401, which made the CLI
  look broken.
- The CLI server-mode path decoded every HTTP response as JSON before checking
  status. Plain-text 401 responses surfaced as `HTTP response JSON decode
  failed`, hiding the actual auth problem.
- Historical zero-byte `agent-bus*.exe` shims in `~/.local/bin` could shadow the
  real binaries in `~/bin` for agents with different PATH ordering.
- Active Claude MCP config used the generic `agent-bus.exe serve --transport
  stdio` path, `localhost` Redis, and stale six-tool documentation. The current
  dedicated MCP binary is `~/bin/agent-bus-mcp.exe`.
- Home coordination docs mixed POSIX environment assignment syntax, private
  numeric IP examples, old source paths, and stale MCP tool counts.
- Remaining warnings are outside the active same-machine agent-bus path:
  broader Claude MCP config still includes private numeric IPs for non-agent-bus
  servers.

## Implemented Resolution

- Added `scripts/sync-agent-bus-client-auth.ps1` to copy the service bearer
  token from the AgentHub service registry into `~/.config/agent-bus/config.json`
  without printing the secret.
- Extended `scripts/validate-agent-client-configs.ps1` to check the installed
  service auth state, client auth availability, loopback URL consistency,
  zero-byte install shadows, MCP examples, and home agent config drift.
- Updated `scripts/invoke-agent-bus-cli.ps1` to prefer `~/bin/agent-bus.exe` and
  reject zero-byte candidates.
- Updated `scripts/setup-agent-hub-local.ps1` to run client config validation
  after local setup and point operators at the auth sync script.
- Changed the HTTP service auth failure response to JSON with a
  `WWW-Authenticate: Bearer` header.
- Changed CLI server-mode HTTP handling to read the body first, check status,
  and explain missing `AGENT_BUS_AUTH_TOKEN` or `auth_token` for 401 failures.
- Updated repo docs and active home agent coordination docs to use the dedicated
  MCP binary, IPv4 loopback Redis/PostgreSQL URLs, current 17-tool MCP surface,
  and the same-machine auth sync workflow.
- Removed confirmed zero-byte `agent-bus*.exe` shims from `~/.local/bin`.

## Validation

- `scripts/sync-agent-bus-client-auth.ps1 -DryRun` confirms it can update the
  same-machine client config from the installed AgentHub service.
- `scripts/validate-agent-client-configs.ps1` passes after removing zero-byte
  shims. It still reports warnings for legacy Claude/private-IP config drift.
- Live installed CLI smoke passed:
  - `agent-bus health --encoding compact`
  - `agent-bus send --from-agent codex --to-agent codex --topic diagnostic ...`
  - `agent-bus read --agent codex --since-minutes 30 --limit 5 --encoding compact`
- Targeted Rust validation passed:
  - `cargo test -p agent-bus --features server-mode server_mode::tests --lib`

## Remaining Work

- Keep `~/.claude.json` and `~/.claude/mcp.json` on stdio MCP for agent-bus;
  do not reintroduce inline bearer tokens or HTTP authorization headers.
- Decide whether `watch` should honor server-mode HTTP/SSE instead of direct
  Redis only.
- Optimize large history paths: `/messages`, `journal`, and export currently
  over-fetch and filter client-side for some selectors.
- Add a dedicated server-mode smoke script that exercises health, authenticated
  send/read, presence, and failure messaging for both local and remote URL
  profiles.
- Implement the multi-host backend direction in
  [`multi-host-backend-plan-2026-06-26.md`](./multi-host-backend-plan-2026-06-26.md).
