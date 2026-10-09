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
- a claim is granted only through an authoritative route.

Run the cloud tier separately: `/health` must answer, and authenticated
calls need a cloud-tier token. Cloud tokens are a separate credential space
(see `cloud/agentbus/README.md`). Until hub-to-cloud sync runs, the cloud
tier is its own store, not a view of the on-site hub.
