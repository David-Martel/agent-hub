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
2. **Keeps stores ordered, races their aliases.** The resolver exhausts the
   authoritative tier before independent fallback stores, then the cloud tier.
   Within a tier, independent stores retain their first configured position.
   Only candidates with the same role and the same explicit, nonempty `hub`
   name form an interchangeable route group. Each unnamed candidate is a
   separate store; two healthy URLs alone do not prove shared storage.

   Within one group, detected sites and link class order the primary routes.
   Their probes race with the configured class stagger. If none succeeds, the
   group's deferred routes are tried before moving to an independent store.
   A faster cloud response cannot bypass a healthy authoritative store.

   DNS suffixes come from DHCP, so any network can claim to be any site.
   Location hints only reorder the operator's declared aliases within a
   store. They never change the role/store barrier or permanently discard a
   deferred route.
3. **Keeps authority fixed.** A candidate's role never depends on location.
   Several `authoritative` candidates are accepted only when they all name the
   same `hub`, which makes them alternate routes to one claims authority.
   Claims consider only authoritative routes and still verify the named hub's
   reported identity; a successful route can end the remaining probe race.
4. **Checks the route label.** A hub configured with `hub_identity` reports
   it in `/health`. A candidate that names a `hub` is used only when the
   probed hub reports exactly that identity. A missing identity counts as a
   mismatch, so a dead tunnel whose port was reused, or an older or unrelated
   hub, can never become a claims authority. Candidates without a `hub` keep
   accepting any healthy hub.

   This is a routing check, not authentication. The probe already carries the
   candidate's credential, and any process can report any identity.
5. **Caches route preferences within a store.** The last-good route cache uses
   the candidates, detected location set and network-prefix fingerprint. A
   changed attachment key causes a fresh resolution. A cache hit can promote
   an alias inside its own store group, but cannot move a fallback or cloud
   store ahead of authority.

   The route-cache default remains 60 seconds. This is the entry age since
   the last successful resolution, which refreshes its timestamp, rather than
   a guaranteed periodic race. Repeated healthy cache hits can retain an alias
   for longer. The fingerprint tracks distinct prefixes, not interface identity:
   adding an Ethernet interface on the same prefix as an active Wi-Fi interface
   need not change the key. This change does not guarantee detection of every
   docking or interface transition.

   Long-lived MCP dispatch also has a separate, process-wide backend cache in
   [`remote_dispatch.rs`](../crates/agent-bus-core/src/remote_dispatch.rs). Its
   existing 15-second lifetime is keyed by candidates, not network attachments.
   A previously selected backend can therefore remain selected for up to that
   lifetime after a network change, even though a newly run resolver would
   choose a different route. An actual remote-call failure invalidates both
   caches immediately; cache invalidation itself does not replay the failed call.

`agent-bus health` and MCP `bus_health` report `network_locations` beside
`backend`. They also report `network_location_error` when the rules were
invalid and ignored. Invalid location hints leave the role/store barrier and
within-store link-class ordering intact instead of taking the client offline.
The shared implementation is in [`hub.rs`](../crates/agent-bus-core/src/hub.rs)
and [`link_route.rs`](../crates/agent-bus-core/src/link_route.rs).

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
    {"url": "http://127.0.0.1:18480", "role": "authoritative",
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

The default local port is 18480. Port 18400 is taken on Windows hosts that run
the NSSM `AgentHub` local maintenance hub, which must never be a candidate.

### Keeping the forward up

A forward started by hand dies with its session, and the bus then drops for every
agent on that machine. Each helper is idempotent: it exits 0 when the loopback
`/health` already answers, so re-running it on a timer restarts a dead forward and does
nothing otherwise. On a timeout it stops only the ssh process it started. Keep the jump
host in `~/.ssh/config` and pass only the alias.

Windows: a logon task that re-runs every 5 minutes for the logged-on user.

```powershell
$action  = New-ScheduledTaskAction -Execute 'pwsh.exe' -Argument (
  '-NoLogo -NoProfile -WindowStyle Hidden -File "<repo>\scripts\agent-bus-tunnel.ps1" ' +
  '-Jump <ssh-alias> -Target <hub-host>:8400')
$trigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME
$trigger.Repetition = (New-ScheduledTaskTrigger -Once -At (Get-Date) `
  -RepetitionInterval (New-TimeSpan -Minutes 5)).Repetition
$settings = New-ScheduledTaskSettingsSet -MultipleInstances IgnoreNew -StartWhenAvailable `
  -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit ([TimeSpan]::Zero)
Register-ScheduledTask -TaskName 'AgentBusTunnel' -User $env:USERNAME `
  -Action $action -Trigger $trigger -Settings $settings
```

The battery and time-limit settings matter on laptops. By default Task Scheduler does not
start the task on battery, kills it when AC is unplugged, and stops it after 72 hours. While
ssh is alive the task stays Running, so the 5-minute repetition is skipped. When ssh dies,
the next tick restarts it. Check once with `Get-ScheduledTask AgentBusTunnel` that the task
shows Running while the forward is up.

Linux (systemd user unit). systemd supervises the forward itself, so the helper is
not needed. Do not wrap the helper in a `Type=oneshot` unit: systemd kills the
backgrounded ssh when the oneshot exits.

```ini
# ~/.config/systemd/user/agent-bus-tunnel.service
[Unit]
Description=agent-bus SSH forward to the on-site hub
StartLimitIntervalSec=0

[Service]
ExecStart=/usr/bin/ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=30 -o ServerAliveCountMax=3 -L 127.0.0.1:18480:<hub-host>:8400 <ssh-alias>
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
```

Enable it with `systemctl --user enable --now agent-bus-tunnel.service`. Two prerequisites:
`loginctl enable-linger $USER`, or the unit stops at logout. And a key that works without an
ssh-agent, because the user manager does not inherit `SSH_AUTH_SOCK`, and `BatchMode=yes` then
fails on a passphrase-protected key and retries forever. Use either the helper or the unit,
not both: with `ExitOnForwardFailure=yes` the unit fails while the helper's forward holds the
port.

If an existing `config.json` candidate still points at `127.0.0.1:18400` from the old helper
default, move it to `127.0.0.1:18480` together with the forward.

List the
loopback candidate after any direct LAN or fabric candidate, and give it its own
`sites`. Location matching then tries the direct route first wherever one exists.

## Validation matrix

For each client location, check these against the on-site hub:
- `agent-bus health` selects the expected route;
- `network_locations` lists the expected sites;
- an authenticated `read`, a `send` and `presence` succeed;
- authenticated `read-direct --agent-a <local-id> --agent-b <peer-id>` and
  `post-direct --from-agent <local-id> --to-agent <peer-id> --body <message>`
  use the selected hub's private channel, preserving thread IDs and tags;
- a claim is granted only through an authoritative route.

Direct-channel commands follow the same configured candidate selection and
per-candidate bearer authentication as ordinary messages. If every configured
hub is unreachable, they fail explicitly instead of accessing local Redis.
HTTP rejection or an invalid response also fails without reporting success.
With no hub configured, the existing local direct-channel operations remain
available. Builds without `server-mode` retain that local command surface.

Run the cloud tier separately: `/health` must answer, and authenticated
calls need a cloud-tier token. Cloud tokens are a separate credential space
(see `cloud/agentbus/README.md`). Until hub-to-cloud sync runs, the cloud
tier is its own store, not a view of the on-site hub.
