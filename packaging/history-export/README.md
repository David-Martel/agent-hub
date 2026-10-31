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
python3 -B -m unittest scripts/ci/test_history_export.py scripts/ci/test_history_prune.py -v
```

[`scripts/ci/test_history_export.py`](../../scripts/ci/test_history_export.py)
and [`scripts/ci/test_history_prune.py`](../../scripts/ci/test_history_prune.py)
run in CI's Unit Tests step through `unittest discover -s scripts/ci`. They use a
fake `sftp` and need no network, PostgreSQL, Redis or drop host. The prune tests
run the real PowerShell script and skip, saying so, where no `pwsh` or
`powershell` exists.

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
`agent-hub-history-export-notify@.service`, which runs
[`agent-hub-history-export-notify.sh`](agent-hub-history-export-notify.sh) and
posts a SEVERITY high FINDING on `machine-hygiene`. A failed mirror is reported
and does not fail the run.

To prove the alert path works, start the notify unit with an instance name that
contains `positive-control`:

```bash
systemctl --user start agent-hub-history-export-notify@positive-control.service
agent-bus read --thread-id agent-hub-alert-selftest --since-minutes 5 --encoding toon
```

That posts a `POSITIVE-CONTROL` finding at SEVERITY info on its own thread,
`agent-hub-alert-selftest`, never on `machine-hygiene`, so other lanes do not
treat it as an incident. Any other instance name is a real alert.

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

## Retention

The drop account cannot delete anything, so bundles accumulate (about 60 MB a
day in October 2026). [`windows/Invoke-AgentHubPrune.ps1`](windows/Invoke-AgentHubPrune.ps1)
prunes them on each drop host. It runs daily at 03:30 as its own local account,
`agenthubprune`, never as `agenthubdrop`, and keeps:

- the newest 14 bundles by directory creation time;
- every bundle named in another receipt or MANIFEST.json under
  `C:\codedev\agent-hub-artifacts`, or listed in the pin file `incoming\KEEP`;
- every bundle that fails verification, logged as `anomaly-kept`.

Before each delete it checks the bundle name against sha256(SHA256SUMS), every
listed hash, and that no file is unlisted or missing. It then appends a
`remove` line with those hashes to `prune-log\prune.ndjson` and deletes the
bundle and its receipt. A delete that fails adds `remove-failed` with the
error and fails the run, which shows as a nonzero last result on the task.
`-WhatIf` logs `would-remove` instead and deletes nothing.

**Install** on each drop host, elevated, after
[`Install-AgentHubDrop.ps1`](windows/Install-AgentHubDrop.ps1):

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File Install-AgentHubPrune.ps1
```

The installer creates or updates the account, grants it only "Log on as a
batch job", and adds two ACEs on `incoming`: list, read and delete-child on
folders, and read on files. It gives the account Modify on `prune-log` and read
on `C:\ProgramData\agent-hub-prune`, which holds the copy of the script the
task runs. The account cannot change that copy. The drop installer re-adds the
same two ACEs when it rebuilds the `incoming` DACL, and a test keeps the two
installers in step. Each run sets a fresh random password, hands it to Task
Scheduler for the task's password logon, refreshes every other task of the
account, and keeps no copy. The installer then reads the task back and fails
unless the task runs as `agenthubprune` with a password logon.

Two findings from dtm-p1gen7 (2026-10-29) shaped this:

- **S4U is refused.** Registering the task with an S4U logon failed with
  E_ACCESSDENIED, even as SYSTEM. The cause was not found. Register-ScheduledTask
  reported that failure as a non-terminating error, so a check that did not read
  the task back saw success. Password logon works.
- **Remove-Item -Force is denied** under this account, while
  `[IO.Directory]::Delete` succeeds. The likely cause is that `-Force` resets
  attributes first, which needs WriteAttributes, and the account does not hold
  that right. The prune therefore deletes through .NET, and a static test keeps
  it that way, because pwsh on Linux cannot reproduce the ACL.

**Self-test with the real account**: stage a few synthetic bundles under
`incoming\_prune-selftest`, register a second, untriggered task against that
folder, run it, and check the log:

```powershell
.\Install-AgentHubPrune.ps1 -TaskName agent-hub-prune-selftest -NoTrigger -Keep 1 `
    -PruneArguments '-Root "C:\codedev\agent-hub-artifacts\incoming\_prune-selftest" -ArtifactsRoot "C:\codedev\agent-hub-artifacts\incoming\_prune-selftest" -LogPath "C:\codedev\agent-hub-artifacts\prune-log\selftest.ndjson"'
Start-ScheduledTask agent-hub-prune-selftest   # wait until it is no longer Running
(Get-ScheduledTaskInfo agent-hub-prune-selftest).LastTaskResult   # 0
Unregister-ScheduledTask agent-hub-prune-selftest -Confirm:$false
```

Remove the folder and `selftest.ndjson` afterwards. The self-test run also gives
the main task the new password.

**Limits.**

- **Creation-time order.** The order rests on directory creation time. An SFTP
  v3 client can set only access and modification times, so the drop account
  cannot back-date a bundle. Two bundles created in the same second tie, and the
  tie goes to the name.
- **Flooding.** Retention counts bundles. An uploader holding the drop key could
  push many valid bundles and move real ones out of the newest 14, on both hosts,
  since both take uploads under that key. The defences: the key never leaves
  asuspro13, `KEEP` pins any bundle that must stay, each run's `summary` line
  records the bundle count, and the hub's PostgreSQL history can rebuild a
  bundle.
