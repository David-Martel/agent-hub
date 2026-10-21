# Windows hub identity watchdog

The [watchdog](../scripts/agent-bus-watchdog.ps1) checks the client's configured fleet authority. A reachable, configured cloud fallback remains **degraded**; it never becomes the onsite authority. Unknown identities, unknown endpoints and the known local-island fingerprint remain errors.

## Install, build, test and run

Install a reviewed copy at the existing task's script path, preserving the old file and its effective permissions. Compare the current target SHA-256 with the deployment receipt immediately before replacement; refuse concurrent changes. The source holder is this repository, not the deployed `~/bin` copy. No scheduled task or executable upgrade is needed for this script change.

```powershell
# Source inspection/build: this is a PowerShell script with no compilation step.
Get-FileHash ./scripts/agent-bus-watchdog.ps1 -Algorithm SHA256

# Synthetic tests; both runtimes are required because the task uses Windows PowerShell.
powershell.exe -NoProfile -File ./scripts/test-agent-bus-watchdog-fixtures.ps1
pwsh -NoProfile -File ./scripts/test-agent-bus-watchdog-fixtures.ps1

# Run the reviewed deployed script using the existing task's actual interpreter.
powershell.exe -NoProfile -NonInteractive -File "$HOME/bin/agent-bus-watchdog.ps1"
```

The live command contacts the configured hub, writes watchdog receipts and Windows events, and may display a warning. The [fixture script](../scripts/test-agent-bus-watchdog-fixtures.ps1) instead imports only five actual function definitions through the PowerShell parser. It does not execute watchdog bootstrap, live CLI calls or production config/journal reads. Its alert lifecycle uses a unique owned temporary directory and stubs `Show-WatchdogToast` and `Write-EventLog`. No Pester or other test dependency is needed.

## Classifications and notification behavior

| Observation | Result | Script exit |
| --- | --- | --- |
| Named expected identity, Boolean `authoritative: true`, unique matching authoritative candidate | Healthy | 0 |
| Exactly one matching configured cloud-role URL, identity `cloud`, Boolean `authoritative: false` | `DEGRADED_ONSITE_AUTHORITY_UNAVAILABLE`, retained warning | 2 |
| Offline/local backend, unhealthy hub, absent named authority, unexpected identity, unknown endpoint, known local island or missing/invalid authority bit | Critical/error; never promoted | 1 |

Both candidate role and exact selected endpoint must match. Health `ok` must be Boolean `true`; strings, numbers, null and missing values are errors. String values such as `"false"` also do not satisfy the Boolean authority contract. Trailing URL slashes are normalized; candidate queries, paths and identities are not broadly rewritten or guessed. Existing on-campus tunnel recovery remains unchanged.

Every failed assessment refreshes `~/.config/agent-bus/watchdog-last-alert.json` and emits event 8400. The flag file counts `consecutive_failures`; the first two failures only log, because one failed probe is usually a transient route flap, and the operator is notified at the third consecutive failure (about 30 minutes at the 10-minute task interval). The notification is a non-modal Windows toast from `Show-WatchdogToast` (WinRT `ToastText02` raised under the Windows PowerShell AppUserModelID), not `msg.exe`, whose modal box stole focus. An identical message/detail pair suppresses only its duplicate toast for 30 minutes, measured from `last_notified_utc`, the last toast actually attempted, not from the first failure; a changed incident past the threshold notifies immediately. Missing, malformed or future timestamps do not silence a warning. The timestamp records an **attempt**, not proof that Windows delivered the toast.

Recovery writes `watchdog-last-ok.json`, removes the alert flag and emits recovery event 8401 when a previous alert existed. A later recurrence starts a new failure count and notifies at its third consecutive failure. Existing toasts are not forcibly dismissed. The JSON receipt and event history establish recovery even if a previously shown toast remains visible.

The observed task invokes `conhost.exe --headless powershell.exe ...`. Its wrapper may report task result 0 while the script itself emitted an error or degraded exit. Inspect the watchdog receipt/event and script result; do not treat the wrapper result as proof of health. This change does not modify that task wrapper, schedule or principal.

## Regression evidence and scope

The motivating gen7 alert reported `hub identity mismatch` when the resolver selected its configured cloud fallback. A later guarded invocation of the unchanged deployed script revalidated ASUS and cleared the stale alert flag without changing routing or journal bytes. The new classification preserves the underlying loss-of-onsite-authority warning while making its meaning accurate.

The synthetic suite covers authority/fallback decisions, strict negative controls, duplicate suppression and real temporary-file failure/recovery/recurrence transitions. It does not certify hardware links, network throughput, Windows toast delivery, task scheduling, tunnel success, concurrent watchdog instances or cloud replication. Native Windows PowerShell 5.1 and PowerShell 7 runs are required in CI; [the test review register](test-review-register.json) binds reviews to exact source/test hashes.

See [client installation](agent-client-installation.md) for routing and version boundaries and the [repository guide](../AGENTS.md) for isolated QA and review rules. Existing clients with a retained journal must not have their candidate list casually changed to address a watchdog message.
