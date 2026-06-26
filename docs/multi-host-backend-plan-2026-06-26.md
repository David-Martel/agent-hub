# Multi-Host Backend Plan - 2026-06-26

This plan defines how AgentHub should run across `dtm-p1gen7`, `asuspro13`,
Spark hosts, and other VIGIL machines once Headscale/Tailscale provides the
private network path. It favors one reliable primary with tested recovery over
premature multi-primary storage.

## Goals

- Provide one shared coordination and memory surface for Codex, Claude, Gemini,
  and scripts across multiple machines and repos.
- Preserve same-machine MCP startup when the tailnet or hub is unavailable.
- Keep Redis fast and disposable enough for realtime messaging.
- Keep PostgreSQL durable enough for long-term history, journal export, and
  session recovery.
- Avoid storing bearer tokens in `.claude*`, repo docs, or generated bundles.

## Non-Goals

- Do not make agent-bus a sensor, ROS/DDS, model-output, or large artifact data
  plane.
- Do not run Redis multi-primary.
- Do not rely on automatic failover between two mobile devices.
- Do not require every Spark host to run local Redis or PostgreSQL when it is a
  client only.

## Recommended Architecture

### Phase 0: Same-Machine Baseline

`dtm-p1gen7` runs the local Windows `AgentHub` service on `localhost:8400`,
backed by local Redis and PostgreSQL. Claude and Codex use `agent-bus-mcp.exe`
stdio MCP for in-session tools and the `agent-bus.exe` CLI for scripts.

Required properties:

- `AGENT_BUS_REDIS_URL=redis://127.0.0.1:6380/0`
- `AGENT_BUS_DATABASE_URL=postgresql://postgres@127.0.0.1:5300/redis_backend`
- `AGENT_BUS_SERVER_URL=http://localhost:8400`
- Bearer auth copied into `~/.config/agent-bus/config.json`, not printed.
- Config validation passes with `scripts/validate-agent-client-configs.ps1`.

### Phase 1: Tailnet Shared Hub

Expose one AgentHub HTTP endpoint over the Headscale tailnet. The hub must
explicitly allow remote binds; otherwise AgentHub remains reachable only from
the hub itself.

Hub setup:

```powershell
pwsh -NoLogo -NoProfile -File scripts\install-agent-hub-service.ps1 -AllowRemote
$env:AGENT_BUS_SERVER_HOST = "0.0.0.0"
$env:AGENT_BUS_ALLOW_REMOTE = "true"
```

Only use `0.0.0.0` on a tailnet- or firewall-restricted host with bearer auth
configured. Remote clients then set:

```powershell
$env:AGENT_BUS_SERVER_URL = "http://<agenthub-tailnet-name>:8400"
$env:AGENT_BUS_AUTH_TOKEN = "<from local secret store>"
```

Keep Redis and PostgreSQL bound to the hub's loopback interface. Remote clients
should not connect directly to the database or Redis unless they are promoted to
backend operators.

Recommended hub candidates:

| Candidate | Strength | Risk |
|-----------|----------|------|
| `dtm-p1gen7` | Already has validated Windows AgentHub and active Claude/Codex configs. | UM VPN and laptop sleep can interrupt remote clients. |
| `asuspro13` | Linux host near Spark/lab routes and good subnet-router candidate. | Current bus state is less canonical than `dtm-p1gen7`. |
| UDM/always-on lab Linux host | Stable control-plane placement. | More operational risk; avoid until backup/restore is rehearsed. |

Use `dtm-p1gen7` for same-machine coordination now; promote an always-on Linux
host later if availability matters more than workstation-local convenience.

### Phase 2: Durable PostgreSQL

PostgreSQL is the long-term history backend. Hardening order:

1. Enable WAL archiving or periodic base backups.
2. Add a streaming replica over tailnet.
3. Add restore drills that prove a new hub can read the latest message history.
4. Add logical dumps for human-auditable emergency recovery.
5. Consider Patroni or another HA manager only after there are three stable
   control-plane nodes.

Do not treat an untested replica as a backup. Backups need restore evidence.

### Phase 3: Redis Realtime Reliability

Redis remains the realtime queue and presence layer. Hardening order:

1. Enable AOF persistence on the hub.
2. Configure memory limits and stream trimming by topic/session retention.
3. Add one replica for warm restart.
4. Add Redis Sentinel only when three stable voters exist.

Agent-bus should continue to expose explicit degraded state:

- Redis down means realtime coordination is unavailable.
- PostgreSQL down means history and long queries are degraded, but Redis-backed
  send/read can continue.
- Hub HTTP down means clients use local fallback or queue for replay.

### Phase 4: Offline Spool And Replay

Add client-side local spooling before field deployments depend on cross-machine
bus access. The spool should:

- Persist unsent messages as JSONL or SQLite in a per-user local app directory.
- Use UUIDv7 message ids generated client-side.
- Record source machine, source repo, original timestamp, and send attempt
  count.
- Replay idempotently when `AGENT_BUS_SERVER_URL` becomes reachable.
- Mark replayed messages as delayed so summaries can distinguish live from
  recovered coordination.

This makes agent-bus resilient to Headscale, Wi-Fi, VPN, and hub maintenance
without hiding network failures.

## `.claude*` Configuration Standard

- Preferred Claude MCP command: `%USERPROFILE%\bin\agent-bus-mcp.exe` on
  Windows or `~/bin/agent-bus-mcp` on Linux.
- Preferred active config: `~/.claude/mcp.json`.
- Legacy `~/.claude.json` may contain a project-level MCP entry, but it must
  use stdio and must not contain inline `Authorization` headers or bearer
  tokens.
- SessionStart hooks should call the presence hook with a stable id such as
  `claude`.
- Same-machine Redis/PostgreSQL URLs should use `127.0.0.1`, not ambiguous
  `localhost`.
- Cross-machine HTTP URLs should use Headscale DNS names, not private numeric
  LAN IPs, unless the file is explicitly host-local and ignored.

## Validation Matrix

| Scenario | Command | Expected result |
|----------|---------|-----------------|
| Same-machine health | `agent-bus health --encoding compact` | Redis ok; PostgreSQL ok or explicit degraded state. |
| Authenticated send/read | `agent-bus send ...`; `agent-bus read ...` | No 401; message appears once. |
| Claude MCP config | `scripts/validate-agent-client-configs.ps1` | Active `.claude*` agent-bus entries use stdio and dedicated MCP binary. |
| Remote tailnet client | `AGENT_BUS_SERVER_URL=http://<hub>:8400 agent-bus health` | Health reachable over tailnet with auth. |
| PostgreSQL outage | Stop PostgreSQL, then send/read | Redis path works; `database_ok=false` is visible. |
| Redis outage | Stop Redis, then health/send | Failure is fast and actionable. |
| Hub restart | `agent-bus service --action restart` | Health returns; clients reconnect. |
| Tailnet dropout | Stop `tailscaled` on a client | Local MCP still starts; future spool queues messages. |
| Restore drill | Restore latest backup to a fresh DB | `read`/`journal` can query recovered history. |

## Implementation Backlog

- Add `scripts/test-agent-bus-remote-smoke.ps1` for `AGENT_BUS_SERVER_URL`,
  auth, send/read, presence, and SSE notification replay over a tailnet URL.
- Add client-side spool/replay support with idempotent message ids.
- Add `agent-bus service --action backup` and a companion restore validator for
  PostgreSQL history.
- Add retention controls for Redis streams by repo/session/topic.
- Add health output fields for Redis persistence mode, PostgreSQL replication
  lag, backup age, and hub identity.
- Add config validation checks for inline bearer tokens in active `.claude*`
  MCP entries without printing token values.
- Add a documented promotion runbook for moving the writable hub from
  `dtm-p1gen7` to an always-on Linux host.

## References

- Redis persistence:
  <https://redis.io/docs/latest/operate/oss_and_stack/management/persistence/>
- Redis replication:
  <https://redis.io/docs/latest/operate/oss_and_stack/management/replication/>
- Redis Sentinel:
  <https://redis.io/docs/latest/operate/oss_and_stack/management/sentinel/>
- PostgreSQL high availability:
  <https://www.postgresql.org/docs/current/high-availability.html>
- PostgreSQL warm standby and streaming replication:
  <https://www.postgresql.org/docs/current/warm-standby.html>
- PostgreSQL continuous archiving and point-in-time recovery:
  <https://www.postgresql.org/docs/current/continuous-archiving.html>
- Patroni:
  <https://patroni.readthedocs.io/>
- Tailscale subnet-router high availability:
  <https://tailscale.com/docs/how-to/set-up-high-availability>
