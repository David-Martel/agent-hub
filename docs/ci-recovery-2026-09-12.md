# PR59 CI recovery

The preserved branch at `98c2da1` failed Security Audit in run
`32309014841`: h2 `0.4.13` was affected by `RUSTSEC-2026-0258`; the job
prescribed `>=0.4.16`. The Windows job never acquired a runner (runner ID zero,
zero steps) for `self-hosted`, `Windows`, `X64`, `local-build`.

## Dependency repair

Cargo.lock now selects h2 `0.4.16`. A fresh strict audit also discovered yanked
chacha20 `0.10.0`, so it was updated to compatible `0.10.2`. Cargo refreshed five
Windows dependency edges to the already-present windows-sys `0.61.2`; no other
package versions changed. `cargo audit --deny warnings` passes against 1,243
advisories and 350 locked dependencies. Formatting and diff whitespace checks
pass. Exact revised-commit CI supplies the cross-platform build/test evidence;
the old revision's successful jobs do not validate this lockfile.

## Windows runner recovery

The existing `C:/actions-runner/agent-hub` installation identified this exact
repository and runner name `dtm-p1gen7-agenthub-windows-x64`. Its old registration
ID 24 was absent from GitHub; no service, scheduled task or listener remained.
The nonsecret runner identity file was preserved as `.runner.before-20260912`.
The installed runner's supported `remove --local` removed stale registration,
then an ephemeral registration token was passed through the runner's environment
input. No token was printed, stored in this repository, or placed in arguments.

The restored registration is ID 27 with the unchanged name and required Windows,
X64 and local-build labels. GitHub reported it online. Other runners and workflow
OS requirements were unchanged.

The `AgentHub Windows Runner` scheduled task runs the existing
`C:/actions-runner/agent-hub/bin/Runner.Listener.exe` with
`run --startuptype manual`, working directory `C:/actions-runner/agent-hub`.
It uses the current user's interactive logon token at limited run level: no
stored password, no new service account, no elevated task. It starts at that
user's logon, has no execution time limit, ignores duplicate task starts, and
retries failures three times at one-minute intervals. It was started and verified
running after creation. This runner requires the user session to remain logged
on; it is not an unattended boot service.

Do not register a second runner or retarget Windows jobs to Linux to address a
future offline state. First verify the exact task action, process, GitHub runner
identity and labels, and inspect this installation's diagnostic logs without
sharing credentials. The application and service lifecycle are documented in
[GitHub's runner removal guidance](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/remove-runners)
and [Windows service guidance](https://docs.github.com/en/actions/how-tos/manage-runners/self-hosted-runners/configure-the-application?platform=windows).

Local unit validation initially entered CargoTools' automatic fix preflight and
contended with another repository's shared Cargo target. Only that identified
build process tree was stopped; no source files were changed by its partial
preflight. Use the raw CargoTools route and an isolated target, or explicitly
coordinate shared-cache build sequencing. The required repository Lefthook hooks
and deterministic Codex attribution hook were installed in the repository's
existing local hook directory; shared Git guard files were verified unchanged.

## Measured Windows build budget

Run `34711785457` at `1cacddb` passed all 11 Linux jobs, including 789 unit
tests and 60 integration tests. Its first Windows attempt also passed all 789
unit tests before the runner was interrupted during release compilation.
The retry's cold test compilation alone consumed more than 26 minutes, leaving
the 30-minute job budget insufficient for release binaries and the remaining
database, helper, fleet, MCP, and provenance fixtures. The Windows job budget is
now 60 minutes. Every build command, test assertion, runner label, and fixture
remains unchanged; the revised commit still requires its own successful CI run.
