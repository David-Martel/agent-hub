# agent-hub history export

```sh
# install preview; deploy only after the restore qualification below
packaging/history-export/install.sh --dry-run
# build: standard-library syntax check
python3 -B -c 'from pathlib import Path; p=Path("packaging/history-export/agent-hub-history-export.py"); compile(p.read_bytes(), str(p), "exec")'
# test: CI Unit Tests selectors, no live stores
python3 -B -m unittest scripts/ci/test_history_export.py scripts/ci/test_history_prune.py -v
# run: only after reviewed deployment and independent restore qualification
systemctl --user start agent-hub-history-export.service
```

The existing daily job exports the complete hub history to encrypted, owner-only
Google Drive custody. It has no required Windows-host transport. The operator's
2026-11-03 directive to use Google Drive or DTM-WORK replaces the earlier
personal-Drive automation restriction and the old required Gen7/optional Gen9
drop routing. Existing drop backups remain preserved. Up: [packaging README](../README.md).

## Private Drive backup

The [helper](agent-hub-history-export.py) preserves the PostgreSQL repeatable-read
snapshot, Redis raw message/side-channel export, UUID reconciliation and gzip
bundle/checksum format described below. Redis reads are live, not atomic with the
PostgreSQL snapshot. It verifies an exact regular-file census, safe relative paths,
every digest, gzip/NDJSON parsing and line counts before packing. Full private
message bodies remain in the backup; logs and bus notices contain metadata only.
Parsed PostgreSQL message/presence counts must equal both the recorded manifest
snapshot counts and `snapshot.tsv`; the snapshot time and complete field set must
also agree. A self-consistent checksum list alone cannot hide truncated SQL output.

The [existing service](systemd/agent-hub-history-export.service) uses `--drive` and
keeps its flock, failure notification and daily/Persistent timer. Its `uv --offline
--with redis` dependency must already be cached; the pilot checks that prerequisite.
It uses the operator-qualified `/srv/vigil-migration/asuspro13/rclone.conf`, fixed
`migration-drive:` and private folder binding. It reads the transport qualification
and public `age-recipient.txt`, checks the expected owner and public-recipient hash,
and refuses inherited `RCLONE_*` redirects. It never copies credentials or an age
private key. Normal rclone credential refresh uses only the dedicated config; the
separate source migration worker/configuration is not rewritten by this helper.

A tar of the complete bundle is encrypted with `/usr/bin/age`. The object name is
`agent-hub.<history-name>.<cipher-sha256>.tar.age`; the separate `.receipt.json` binds
the snapshot, inner manifest/checksum-list, tar and ciphertext hashes and provider
object. Both objects use immutable copy-to, official owner/permission/parent/size
checks and complete streamed SHA256 readback. No sync, public sharing or remote
delete is used. A provider listing alone is not a successful backup.

Owned work stays under `~/.local/state/agent-hub-history-export/drive-spool/`.
`binding.json` closes an exact tar/cipher pair; retry reads that pair rather than
re-exporting or re-encrypting. Auth/upload/readback failures keep the spool. Unknown
partials, changed bytes or conflicting remote objects are held for review without
overwrite or deletion. A failed or incomplete earlier job is handled before a new
snapshot. The job refuses work below a100GiB free-space reserve and checks additional
archive/encryption capacity. Raw export, bundle, tar and ciphertext are retained
even after cloud proof pending independent restore; no new automated retention
policy is introduced. `verified.json` appears only after full cipher and completion
receipt readback. Small current state is `last-drive-result.json`; per-backup cloud
receipts also live in the existing local `receipts/` directory.

Deploy only the reviewed helper and existing service after isolated tests and
`systemd-analyze --user verify`. The installer enables the timer, so do not run its
non-preview mode before qualification. Keep the timer disabled for the first actual
read-only snapshot/encrypt/upload pilot. Independently download and decrypt that
exact object on an authorized key-custody machine; never place the private key on
the hub. Before extraction reject absolute/traversal/duplicate/link/device tar
members. Extract into a new inert directory, require the exact file census, run
`sha256sum -c SHA256SUMS`, and validate gzip/NDJSON/counts using
`validate_drive_bundle` from the reviewed helper. Bind the recovered tar, manifest
and checksum-list hashes in the qualification receipt. Only then enable the existing
timer and check its next trigger and last success. A verified archive is byte
recoverability evidence; loading it into the live bus is a separate operation.

The [DriveTests selectors](../../scripts/ci/test_history_export.py) use synthetic
bundles/providers/child streams with no real credentials or backends. They reject
shared/wrong-owner objects, mismatched/duplicate names, unsafe/extra files, changed
ciphertext, corrupt/truncated readback and missing receipt proof; they verify
failed-spool retention and exact retry without live export. The POSIX stream control
runs in Linux CI; unsupported platforms report a skip. Selector review bindings live
in the [existing review register](../../docs/test-review-register.json).

## Legacy SFTP drop workflow

The following restricted-drop install, restore and pruning details describe retained
historical backups and the explicit legacy `--dest` CLI. They are not dependencies
of the current Drive service.

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

**Legacy Drive rule superseded.** The old on-demand-only personal-Drive rule was
replaced by the operator's explicit backup-migration instruction. The private,
encrypted and readback-verified current workflow is documented above; older SFTP
copies are not deleted by that change.

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
