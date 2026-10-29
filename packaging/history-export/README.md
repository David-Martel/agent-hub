# agent-hub history export

A daily job on the agent-bus hub exports the complete bus history and pushes it
to a restricted SFTP drop on the dtm-* Windows hosts. It then proves each copy by
reading it back. The owner directive of 2026-10-29 says agent-hub items live on
dtm-* hosts, not on the hub. Up: [packaging README](../README.md).

## Install

On the Windows drop host (once per host; elevated PowerShell, or over ssh as an
administrator with `-EncodedCommand`), run
[`windows/Install-AgentHubDrop.ps1`](windows/Install-AgentHubDrop.ps1). It is
idempotent. Afterwards remove the one-shot restart task it names:
`Unregister-ScheduledTask agenthubdrop-sshd-restart -Confirm:$false`.

On the hub, as the bus user:

```bash
# The drop key has one use and no passphrase, because a timer uses it.
ssh-keygen -t ed25519 -N '' -C "agent-hub-drop@$(hostname)" -f ~/.ssh/agent-hub-drop_ed25519
packaging/history-export/install.sh --dry-run   # preview
packaging/history-export/install.sh             # install the job and enable the timer
```

If the key is regenerated, put the new public key in the `$pub` line of the
installer and re-run the installer on every drop host.

## Build

Nothing to build. The job is one Python file run with
`uv run --no-project --with redis`. The tests need only the standard library.

## Test

```bash
python3 -B -m unittest scripts/ci/test_history_export.py -v
```

[`scripts/ci/test_history_export.py`](../../scripts/ci/test_history_export.py)
runs in CI's Unit Tests step through `unittest discover -s scripts/ci`. It uses a
fake `sftp` and needs no network, PostgreSQL, Redis or drop host.

## Run

```bash
systemctl --user start agent-hub-history-export.service    # one run now
journalctl --user -u agent-hub-history-export.service -n 20 -o cat
systemctl --user list-timers agent-hub-history-export.timer
```

Each run ends with one `agent-hub-history-export:` line that names the bundle,
its counts, the SHA256SUMS hash and a `<host>=<stage>` result per destination.
It also posts that line on the bus (thread `machine-hygiene`, tag
`repo:agent-hub`). A required destination that fails posts a FINDING and makes
the unit fail. That failure also triggers
`agent-hub-history-export-notify@.service`, which posts a SEVERITY high FINDING.
A failed mirror is reported and does not fail the run.

## What a run produces

Bundles are named `history-<first 16 hex of sha256(SHA256SUMS)>`. The name comes
from the content and is never a date, and an existing name is never overwritten.

| Path in the bundle | Content |
|---|---|
| `messages-pg/part-NNN.ndjson.gz` | `agent_bus.messages`, `row_to_json`, ordered by (timestamp, id) |
| `presence-events-pg/part-NNN.ndjson.gz` | `agent_bus.presence_events` |
| `messages-redis-stream/part-NNN.ndjson.gz` | `XRANGE agent_bus:messages`, raw fields, LZ4 bodies as stored |
| `redis-state/` | claims, resource events, direct/group channels, threads, tasks, presence |
| `reconciliation.json` | PG vs Redis message sets by UUID (they differ, so both are kept) |
| `snapshot.tsv`, `MANIFEST.json`, `SHA256SUMS` | snapshot time and counts, per-file sha256/md5/size, the checksum list |

PostgreSQL is read in one `REPEATABLE READ` snapshot. As a positive control, the
snapshot's message count must lie between two live `agent-bus health` readings
taken before and after the export, or the run stops. Gzip parts are written with
`mtime=0`, so the same rows always give the same bytes.

The receipt stays on the hub at
`~/.local/state/agent-hub-history-export/receipts/<name>.json`. A copy goes beside
each verified bundle as `<name>.receipt.json`.

For a decoded, de-duplicated message stream, use `agent-bus export --all` (see the
root [CLAUDE.md](../../CLAUDE.md)). This job keeps the raw stores instead, plus
presence events and Redis state, which `export --all` leaves out.

## The drop

| Property | Setting |
|---|---|
| Account | local user `agenthubdrop`, member of Users only; random password, never stored or printed |
| Login | public key only, from `C:\ProgramData\ssh\agenthubdrop_authorized_keys` (owner Administrators), `restrict` option |
| Confinement | `Match User agenthubdrop`: `ForceCommand internal-sftp`, `ChrootDirectory C:\codedev\agent-hub-artifacts\incoming`, no TTY, no TCP or agent forwarding |
| Write rights | the folder ACE allows list, create file and create folder; file ACEs allow read only, and OWNER RIGHTS is read only |

As a result the drop can add new bundles but cannot overwrite, delete, rename or
re-ACL anything, including its own uploads. The installer validates the new
`sshd_config` with `sshd -t -f` before replacing the live one. It keeps a
backup at `sshd_config.pre-agenthubdrop` and restarts sshd from a one-shot
SYSTEM task, because restarting sshd inside the ssh session would kill that
session first.

Controls checked by hand on both hosts (2026-10-29):

- a shell login is refused (`This service allows sftp connections only`);
- `/..` and `C:/ProgramData` are out of reach from the chroot;
- overwrite and delete are denied;
- readback is byte-equal.

On dtm-p1gen7, rename is also denied and port forwarding is administratively
prohibited.

The key has no `from=` pin. dtm-p1gen7 currently sees the hub at its lab-LAN
address through a NAT fallback, so a fabric/tailnet pin would deny it. Add the
pin back once that path is settled.

## Runbook

**Verify a bundle by hand** on a drop host, as an administrator. The check passes
when `bad=0` and the SHA256SUMS hash starts with the bundle's suffix:

```powershell
$d = 'C:\codedev\agent-hub-artifacts\incoming\history-<suffix>'
(Get-FileHash -Algorithm SHA256 "$d\SHA256SUMS").Hash
$bad = 0
foreach ($l in Get-Content "$d\SHA256SUMS") {
    $want, $rel = $l -split '  ', 2
    if ((Get-FileHash -Algorithm SHA256 (Join-Path $d $rel)).Hash -ne $want.ToUpper()) { $bad++; $rel }
}
"bad=$bad"
```

**Drive is an on-demand copy, never automated.** No timer, unit or script copies
bundles to the owner's personal Google Drive. The 2026-10-28 rule forbids
automation on personal credentials, and the two dtm-* copies already give
redundancy. When the owner asks for a Drive copy, an agent runs one
`rclone copy` (never `sync`) of the named bundle to
`gdrive-personal:VIGIL_DATA/agent-hub/history/<name>`, checks it with
`rclone check --one-way`, and records the copy on the bus.

**Restore**: copy a bundle back, run `sha256sum -c SHA256SUMS` in it, then read
the gzip NDJSON parts. MANIFEST.json's `body_encoding` field explains the Redis
LZ4 bodies.

**Retention**: the drop cannot delete anything, so bundles accumulate (about
60 MB a day in October 2026). An administrator-side prune runs separately from
the drop account.
