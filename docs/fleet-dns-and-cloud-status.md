# Fleet DNS + Cloudflare hub status (2026-10-08)

## Recovered plan (Claude / ASUS / #79)

The durable design is **not** “pick one static LAN IP forever.” Multi-homed
laptops (`dtm-p1gen7`, `dtm-carbon-two`) leave UM LAN / lab LAN regularly, so
static `.50.x` addresses are only one site.

Canonical pieces already in-tree:

| Artifact | Role |
|---|---|
| [agent-hub#79](https://github.com/David-Martel/agent-hub/issues/79) | Roaming-safe ordered hubs, offline outbox, off-site Cloudflare hub/sync |
| `docs/network-location-routing.md` | CIDR + DNS-suffix site detection; `sites`/`hub` on candidates; SSH forward on `:18480` |
| `docs/multi-host-backend-plan-2026-06-26.md` | Headscale/tailnet names over numeric private IPs; single claims authority |
| `docs/per-candidate-hub-auth-20261004.md` | Candidate auth tiers; cloud ≠ on-site token space |
| `cloud/agentbus/README.md` | Worker at **`https://agentbus.dtmventures.com`**; DO SQLite; hub/agent/operator tokens |
| PR #110 (`feat/cloud-sync`) | Hub-side sync task steps 0–1 (draft; smoke red) |
| PR #113 | Wrangler/sharp build hygiene for cloud package |

### Intended route table (by site)

| Client site | Preferred path | Authority |
|---|---|---|
| lab-lan (`192.168.50.0/24`) | `http://agent-bus-hub.vigil.lan:8400` (name) → IP fallback | asuspro13 |
| lab-fabric (`10.60.0.0/16`) | `http://asuspro13-p2p-<client>:8400` | asuspro13 |
| umich / home-lan (suffix) | `http://localhost:18480` via SSH forward | asuspro13 |
| elsewhere | `https://agentbus.dtmventures.com` **cloud role only** until sync | cloud store |

Claims stay on the **on-site** hub until federation is proven. Cloud is a
separate credential and store (`AGENT_BUS_TOKENS`, never on-site hub token).

## What landed vs gaps

| Item | Status (2026-10-08, p1gen7) |
|---|---|
| Network-location client code | In main (health reports `network_locations`) |
| ASUS hub identity `asuspro13` | Live |
| Cloudflare Worker hostname | Resolves; `/health` public |
| Cloudflare MCP / sync | Incomplete — MCP often 501; no local `cloud-token`; #110 not merged |
| Hosts pin drift | **Was** `.79` while hub on `.2` — **temporarily fixed on p1gen7** to `.2` for `asuspro13.local` / `agent-bus-hub.vigil.lan` |
| Name-first client config | Applied on p1gen7; health selects `http://agent-bus-hub.vigil.lan:8400` |
| Headscale MagicDNS as sole long-term LAN | Documented; keep VPN name `asuspro13.vpn.dtmventures.com` (100.64.0.3) as optional candidate once verified on-path |

## p1gen7 temporary hosts change (receipt)

- Backup: `%TEMP%\hosts.bak-pre-hub-dns-*`
- Mapping: `agent-bus-hub.vigil.lan` + `asuspro13.local` → `192.168.50.2`
- Verify: `Test-NetConnection asuspro13.local -Port 8400` → True; health
  `backend.url=http://agent-bus-hub.vigil.lan:8400`, `hub_identity=asuspro13`

**Long-term:** prefer DHCP/DNS or Headscale names so laptops do not depend on
hosts edits. ASUS owners should still record A/B/C decision (re-IP vs maps vs
split name) for other fleet hosts.

## Cloudflare next steps (priority)

1. Review federation reliability and authorization separately; #110 remains draft until crash-safe ingestion, legacy claims and concurrent presence are validated. Deploy a reviewed, green main revision only.
2. Preserve the complete existing write-only token map and verified recovery copy before additive token minting or rotation. Validate private permissions before storing
   0600; install `~/.config/agent-bus/cloud-token` for client cloud candidate.
3. Enable hub→cloud sync on ASUS only after token, route, replay, restart, cursor, no-echo and off-site filtering gates pass.
4. Keep cloud candidate `role: cloud` — never authoritative for claims.
5. Re-test off-LAN: health + authenticated send/read with cloud token; expect
   store isolation until sync catches up.

## Coordination

- Thread: `fleet-dns-hub-20261008`
- Related: #79, #80 (outbox), #110, #113
- Quality doctrine: `docs/QUALITY_GATES.md`
