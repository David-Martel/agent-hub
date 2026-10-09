# Warp fleet completion ledger

Observed 2026-10-09 UTC. Owner: Codex integration lane. This is the current
entrypoint; the fleet audit and DNS status retain their dated observations.

## Integration queue

| Item | Current evidence | Owner and next action |
| --- | --- | --- |
| Tunnel ownership #111 and timeout #112 | Both already merged; deployment parity remains separate | Codex: qualify installed revision |
| Cloud dependency #113 | Dependency/lock changes already merged in #114; remaining install-script approvals reviewed | Codex: await exact-head Windows CI |
| Fleet documentation #115 | Stable-path migration, collision checks and original hashes in fleet-path-migration.json | Codex: independent review and exact-head CI |
| Inbox draft #102 | Old captured WIP; current inbox and dispatch/hook replacements must be compared before closure | Codex: preserve original tip and audit unique work |
| Cloud federation #110 | Draft; dedup marker before append can lose messages, claim-key migration and presence-write race need repair | Cloud-sync owner/Codex: repair and disposable-backend regressions |
| Direct routing #117 | Source-owner lane active; independent review found no blocking source issue | Direct-fix owner: finish current-head CI and integrate |
| Dependabot #118 | Active Utils integration lane; skipped CI jobs admitted without explicit provenance | Utils owner: fail closed and validate current-head CI |
| Cloud-token toolkit WIP | Root dirty Rust wiring plus PowerShell/Python prototypes; custody requested | Active writer/Codex: enforce private writes, full-map preservation and hard recovery gates |

## Fleet acceptance

Deployment must use one reviewed main revision, preserve the previous binaries
and configuration, and verify downloaded artifact hashes and embedded revision
for CLI, HTTP and MCP separately. A hash proves byte identity, not health.

Fresh read-only discovery found ASUS and both Sparks running revision a0dea9d;
Carbon CLI reports 3007fcd. ASUS is the authority and its HTTP service runs in
the user systemd scope. Carbon uses an SSH forward on localhost:18480 and has
no AgentHub Windows service. Existing active agents and locked publication
checkouts remain protected.

For each target, capture CLI route/authority/identity, MCP initialize, tools/list
and bus_health from a fresh process, and a scoped nonce send/read round trip.
For ASUS also verify service process identity, maintenance state and database
readiness. Tests run only with disposable or closed-port backends.

Update config/fleet/agent-bus-fleet-v1.json only after the exact target revision
is qualified. Its fd70c8d expectation is historical and is not a deployment
receipt. Add Carbon to the fleet validator when its configuration and target
paths are reviewed. Keep the local Windows maintenance island out of client
candidate lists.

The fleet doctor accepts both hexadecimal/date and Git-describe provenance,
comparing the complete reported revision token with ExpectedBuildRevision;
abbreviations are not treated as evidence of a full revision. It observes the
CLI's selected route without overriding routing variables and checks the hub
identity and authority. Candidate token-file checks inspect file metadata only.
An explicit token_env source still needs separate host-specific qualification.
For a local authority backend, the doctor identifies that narrower evidence;
the HTTP listener and fresh MCP process need the separate probes above.
Carbon remains a manual target until SSH-to-Windows transport is implemented
and independently qualified. The desired-state manifest is unchanged.

Cloud public health is insufficient for fleet fallback. Claims remain on-site;
cloud production acceptance requires token-map recovery, role-specific auth,
federation replay/no-echo/no-offsite, source/deployment parity and the network
policy owner's disposition. Issue #80 outbox and general structural/history
backlog remain separate tasks.

## Cleanup predicates

Fetch current refs, compare PR and deployed heads, preserve unique tip/WIP,
check operations, ignored artifacts, locks, stashes and active writers, then
retire redundant worktrees normally. Never force-remove a locked release tree.
The primary Windows checkout has active cloud-token WIP and must not be reset
or switched underneath its writer. Reconcile this ledger after each disposition.
