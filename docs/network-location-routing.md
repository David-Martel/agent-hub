# Network-location-aware hub routing

Follow-on to agent-hub#79 (roaming-safe agent-bus) and #93 (per-candidate
roles and credentials).

Operator machines move between the lab fabric, campus (on-site LAN or VPN)
and home networks. Each network has a different working path to the same
on-site hub:

| Where the client is | Working path to the on-site hub |
|---|---|
| On the lab fabric | Direct fabric address |
| On campus or the campus VPN | SSH local forward through a campus jump host, on a loopback port |
| Elsewhere (for example a home network) | Usually none, so the cloud tier only |

Without location awareness the client probes every candidate in one fixed
order. Each dead path costs a connect timeout on every CLI or hook call.

## What the client does

1. **Detects its location.** It reads the active interface addresses and the
   connection-specific DNS suffixes:
   - Linux reads the `search`/`domain` lines of resolv.conf.
   - Windows reads the per-interface `Domain`/`DhcpDomain` values, counting
     only interfaces whose address is currently up.

   Each `network_locations` rule that matches contributes its `name`.
2. **Orders candidates.** Candidates whose `sites` include a detected location
   move to the front. Every other candidate follows in configured order, and
   every candidate is still probed if earlier ones fail.

   DNS suffixes come from DHCP, so any network can claim to be any site.
   Detection can therefore only *promote* a route the operator declared for
   that site. It never demotes the configured preference among the rest, and
   with no match the configured order is kept exactly.
3. **Keeps authority fixed.** A candidate's role never depends on location.
   Several `authoritative` candidates are accepted only when they all name the
   same `hub`, which makes them alternate routes to one claims authority.
   Claims probe every such route in location order.
4. **Checks the route label.** A hub configured with `hub_identity` reports
   it in `/health`. A candidate that names a `hub` is used only when the
   probed hub reports exactly that identity. A missing identity counts as a
   mismatch, so a dead tunnel whose port was reused, or an older or unrelated
   hub, can never become a claims authority. Candidates without a `hub` keep
   accepting any healthy hub.

   This is a routing check, not authentication. The probe already carries the
   candidate's credential, and any process can report any identity.
5. **Caches per location.** The last-good route is cached under the candidate
   fingerprint plus the location set. A route learned at home is never tried
   first on campus.

`agent-bus health` and MCP `bus_health` report `network_locations` beside
`backend`. They also report `network_location_error` when the rules were
invalid and ignored. Location only orders probes, so a bad rule set falls
back to configured order instead of taking the client offline.

## Configuration

Keep real networks, addresses and host names out of this public repository.
They belong in each machine's `~/.config/agent-bus/config.json`.

```json
{
  "network_locations": [
    {"name": "lab-fabric", "cidrs": ["10.0.0.0/16"]},
    {"name": "campus", "dns_suffixes": ["campus.example.edu"]},
    {"name": "home", "dns_suffixes": ["home.example.com"]}
  ],
  "server_urls": [
    {"url": "http://hub-fabric-name:8400", "role": "authoritative",
     "hub": "onsite-hub", "sites": ["lab-fabric"]},
    {"url": "http://127.0.0.1:18400", "role": "authoritative",
     "hub": "onsite-hub", "sites": ["campus"]},
    {"url": "https://cloud.example.com", "role": "cloud",
     "token_file": "~/.config/agent-bus/cloud-token"}
  ]
}
```

On the hub itself, set `"hub_identity": "onsite-hub"`. You can also set it
with the `AGENT_BUS_HUB_IDENTITY` environment variable.

Rules for the new fields:

- **Server identity:** a `hub_identity` that clients could never match (bad
  characters, stray spaces) is dropped rather than served.

- **Rules:** each rule needs a `name` (`[A-Za-z0-9._-]{1,64}`) and at least
  one `cidrs` or `dns_suffixes` entry. Unknown keys are rejected.
- **Suffix matching:** a DNS suffix matches on a label boundary, so
  `example.edu` matches `ads.example.edu` but not `notexample.edu`.
- **Override:** `AGENT_BUS_NETWORK_LOCATION=campus,home` replaces detection.
  Use `none` to mean "no known site". This works even without rules.
- **`hub` field:** this is a route label checked against `/health`. It is not
  a credential.

**Rollout order matters.**

1. Upgrade the hub and set its `hub_identity`. A candidate naming a `hub` is
   rejected until the hub reports that identity.
2. Upgrade each client's binaries.
3. Then add `sites` and `hub` to that client's candidates.

A client older than this change rejects a candidate object containing
`sites` or `hub`, which takes it offline. It ignores unknown top-level keys,
so `network_locations` and `hub_identity` are harmless to older binaries.

## SSH forward helper

- `scripts/agent-bus-tunnel.ps1` (Windows)
- `scripts/agent-bus-tunnel.sh` (Linux and macOS; Windows OpenSSH does not
  support `ssh -f`)

Each helper starts `ssh -N -L 127.0.0.1:<port>:<hub>` through a jump host,
and only when nothing already answers on the loopback `/health`. It then
waits for the forwarded hub to answer, and running it again is a no-op.

A forward that never answers is stopped on timeout, error or interrupt.
Jump destinations starting with `-` are rejected, and the destination is
passed after `--`.

The client sends its on-site token to loopback candidates, which is
unchanged posture. Do not run a forward on a shared multi-user machine.

## Validation matrix

For each client location, check these against the on-site hub:
- `agent-bus health` selects the expected route;
- `network_locations` lists the expected sites;
- an authenticated `read`, a `send` and `presence` succeed;
- a claim is granted only through an authoritative route.

Run the cloud tier separately: `/health` must answer, and authenticated
calls need a cloud-tier token. Cloud tokens are a separate credential space
(see `cloud/agentbus/README.md`). Until hub-to-cloud sync runs, the cloud
tier is its own store, not a view of the on-site hub.
