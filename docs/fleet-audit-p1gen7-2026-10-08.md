# Fleet audit — dtm-p1gen7 + asuspro13 (2026-10-08)

**Author:** warp-oz on dtm-p1gen7  
**Repo:** `David-Martel/agent-hub` checkout `C:\codedev\agent-bus`  
**Purpose:** Durable handoff for other agents. Prefer falsifiable checks over green-but-empty tests.

## Live authority (verified)

| Check | Result | How verified |
|---|---|---|
| Authoritative hub URL | `http://192.168.50.2:8400` | `agent-bus health --encoding json` → `backend.url`, `backend.authoritative=true` |
| Hub identity | `asuspro13` | health `hub_identity` |
| Hub build | `0.5.0 (a0dea9d… 2026-10-06)` | health `hub_build` / ASUS `~/.local/bin/agent-bus* --version` |
| TCP `.2:8400` | OPEN | `Test-NetConnection` / curl health |
| TCP `.79:8400` | CLOSED / timeout | same |
| Local island `:18400` | OPEN (NSSM AgentHub) | maintenance only — **never** a `server_urls` candidate |
| Tunnel port `:18480` | OPEN on loopback | tunnel helper target for umich/home-lan |
| Warp MCP `bus_health` (both servers) | remote ASUS authority | MCP tool call 2026-10-08 |

Repo HEADs matched at audit time: local + ASUS `main` = `3007fcd`.

## ASUS runtime (verified over SSH to 192.168.50.2)

- **Service:** `systemctl --user agent-bus-http.service` **active + enabled**, PID matched health maintenance pid, `Linger=yes`.
- **Not** a system unit named `agent-bus-http.service` (system-level check falsely fails).
- **Relay:** `agent-bus-relay-dtm.service` inactive (expected per fleet manifest).
- **Config mode:** `~/.config/agent-bus/config.json` and `hub.env` mode `600`.
- **Worktrees present** for open PR branches: cloud-sync, hub-transport-flake, tunnel-own-pid, wrangler-sharp; locked release worktree `2525c4d`.

## DNS / name / IP decision (NEEDS ASUS + owner)

### Facts

1. Live hub LAN address is **`192.168.50.2`** (health + TCP + curl).
2. Windows hosts on p1gen7 still pins:
   ```
   192.168.50.79 asuspro13-lan asuspro13.local … agent-bus-hub.vigil.lan
   ```
3. `asuspro13.local` therefore resolves to **`.79`**, which does **not** answer `:8400`.
4. SSH `Host asuspro13` → `HostName asuspro13.local` inherits the bad pin; direct `damartel@192.168.50.2` works.
5. Client `config.json` entry 1 is interim **hardcoded** `http://192.168.50.2:8400` (by design until names work).

### Options (ASUS agents: pick one and post decision)

| Option | Action | Pros | Cons |
|---|---|---|---|
| **A. Re-IP hub to `.79`** | Move asuspro13 LAN back to historical `.79` | Hosts + old docs keep working | Needs DHCP/static change on ASUS; brief hub downtime |
| **B. Update maps to `.2`** | hosts on p1gen7 (and any other clients) → `.2`; then replace config URL with `http://asuspro13.local:8400` or `http://agent-bus-hub.vigil.lan:8400` | Matches current DHCP | Must touch every client hosts/DNS |
| **C. Split names** | Keep `asuspro13.local` for general host; introduce dedicated hub name already correct | Least surprise for non-bus tools | Still need one correct hub name everywhere |

**p1gen7 will not unilaterally change hosts** until ASUS posts a decision on thread `fleet-dns-hub-20261008` (see bus messages). Easy interim remains IP in `server_urls`.

## MCP client alignment

| Client | Before audit | Gold target | p1gen7 action 2026-10-08 |
|---|---|---|---|
| Claude | `agent-bus-mcp-aa33879…` + `AGENT_BUS_CONFIG` | same | leave |
| Codex | same | same | leave |
| Gemini | `agent-bus.exe serve` + local Redis/PG env | MCP binary + `AGENT_BUS_CONFIG` only | **FIXED** (backup `mcp_config.json.bak-pre-gold-agentbus-*`) |
| Warp | live health → ASUS | keep | leave |
| Repo `examples/mcp/*` | localhost island recipes | document remote client pattern | still open |

## Binary / fleet drift

| Surface | SHA / note |
|---|---|
| p1gen7 `agent-bus.exe` (generic) | reports `2525c4d` |
| p1gen7 Claude/Codex MCP pin + NSSM http | `aa33879…` |
| ASUS hub binaries | `a0dea9d` |
| `config/fleet/agent-bus-fleet-v1.json` `expected_build_revision` | **`fd70c8d` STALE** — update after next atomic deploy |

Deploy rule (from `mcp-schema-defects.TODO.md`): rebuild/install **cli + http + mcp** from one SHA; fail deploy if SHAs disagree.

## Env hygiene

- `AGENT_BUS_SERVER_URL(S)` User/Machine: empty (good).
- User `AGENT_BUS_AUTH_TOKEN`: still set; duplicates config and overrides it. **Recommended remove** after config-only health proof (owner decision).

## Open PRs (GitHub `David-Martel/agent-hub`)

| PR | State | CI signal (snapshot) |
|---|---|---|
| #113 wrangler/sharp | MERGEABLE | Windows Build pending |
| #112 hub-transport flake | MERGEABLE | Windows Build pending |
| #111 tunnel kill own pid | MERGEABLE | Windows Build pending |
| #110 cloud-sync (draft) | MERGEABLE | **CLI/HTTP Smoke FAILED** |
| #102 filtered inbox (draft) | **CONFLICTING** | multiple failures |

Local staged WIP on branch `fix/direct-channel-hub-routing` (+348 on hub routing tests/docs) sits on top of `main@3007fcd` — do not assume it is merged.

## Validation doctrine (anti-reward-hack)

Treat as **insufficient**:

- Unit tests that never hit a live hub or disposable backends.
- `ok: true` without checking `hub_identity`, `backend.mode=remote`, and `backend.url`.
- MCP initialize alone without a tools/list + one scoped read/write.
- Systemd system-unit probes on ASUS (wrong scope).

Treat as **credible**:

1. `agent-bus health --encoding json` → identity + authoritative URL + `database_ok` + `pg_dropped_writes=0`.
2. TCP/curl to the **resolved** hub name and to the **IP**.
3. MCP: initialize → tools/list → `bus_health` with same identity as CLI.
4. `send` + `read --agent <id> --tag repo:agent-hub --since-minutes 5` round-trip.
5. Presence shows expected hosts (`asuspro13` metadata where claimed).
6. Isolated `scripts/test-agent-bus-isolated.sh` / disposable Redis+PG for code changes — never live `:6380/:5300/:8400` as test backends.

## Recommended next steps

1. **ASUS:** decide DNS option A/B/C on thread `fleet-dns-hub-20261008`; post chosen IPs and who updates DHCP/hosts.
2. **p1gen7:** after decision, apply hosts (if B) and switch config entry 1 from IP to name; re-run health.
3. **All agents:** use MCP gold pattern; avoid local Redis env on clients.
4. **Merge train:** #111–#113 when Windows green; triage #110/#102 separately.
5. **Atomic redeploy** one SHA fleet-wide; refresh fleet JSON `expected_build_revision`.
6. **Cloud:** keep non-authoritative until federation + token + MCP routes proven (#110).

## Message IDs / coordination

Bus posts from this run use:

- `repo:agent-hub`
- `session:fleet-audit-20261008`
- `thread_id: fleet-dns-hub-20261008`
- topics: `status`, `coordination`
- schema: `status`

Agents on asuspro13 should `read` / `read-direct` and reply with a DNS decision before p1gen7 changes hosts.

## Verification receipts (same day, after easy wins)

| Gate | Result |
|---|---|
| CLI health | `hub_identity=asuspro13`, `backend.url=http://192.168.50.2:8400`, `authoritative=true`, `database_ok=true` |
| Process `AGENT_BUS_AUTH_TOKEN` cleared | health still ASUS (config token path works) |
| Gemini MCP config | gold binary + `AGENT_BUS_CONFIG` written; backup `mcp_config.json.bak-pre-gold-agentbus-*` |
| Gold MCP initialize/tools/list/`bus_health` | exit 0; tools include `bus_health`; body has `asuspro13` + `192.168.50.2` + authoritative |
| Bus broadcast + knocks | posted as `warp-oz-p1gen7`; nonce `c9af5909` |
| Read roundtrip | `--from-agent`, `--repo agent-hub`, and `--thread-id fleet-dns-hub-20261008` all recover nonce |
| Hosts change | **not applied** — waiting ASUS A/B/C |

### Discovered CLI/hub friction (real issue, not a greenwashed test)

- `agent-bus read --tag repo:agent-hub` returned **HTTP 400** against the live hub.
- Prefer `--repo agent-hub` / `--from-agent` / `--thread-id` until tag query path is fixed or client/server SHAs match.
- This is exactly the class of defect reward-hacked suites miss if they only assert local unit success.

## Update 2026-10-08 evening (p1gen7)

- Hosts: hub aliases remapped .79 → .2 (backup under %TEMP%\hosts.bak-pre-hub-dns-*).
- Client config: name-first server_urls; health selects `http://agent-bus-hub.vigil.lan:8400`.
- Plan recovery doc: `docs/fleet-dns-and-cloud-status-2026-10-08.md`.
- Quality: `docs/QUALITY_GATES.md` + `.qa-gate.conf` for git-guard overlay.
