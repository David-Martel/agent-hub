# Warp fleet completion ledger

Observed 2026-10-10 UTC. Owner: Codex integration lane. This is the current
entrypoint; the fleet audit and DNS status retain their dated observations.

## Integration queue

| Item | Current evidence | Owner and next action |
| --- | --- | --- |
| Tunnel ownership #111 and timeout #112 | Both merged; installed revisions remain separate evidence | Codex: qualify final installed revision |
| Cloud dependencies #113 and #114 | Both merged; current dependencies and lockfile are accepted | Complete; preserve their original receipts |
| Fleet documentation #115 | Merged as cf7397a; stable-path migration and original hashes preserved | Complete source work; qualify final deployment |
| Inbox isolation #102 | Merged as a6bf24d after preserving and porting useful original work | Complete; disposable backend/history gates remain mandatory |
| Cloud federation #110 | Merged as d4edb4c with atomic ingestion and disposable backend regressions | Source accepted; verify final on-site sync runtime |
| Direct routing #117 | Merged as 3b7cd9c with configured-hub routing checks | Complete source work; verify installed clients |
| Windows GNU/Wine #120 and #121 | Both merged; #121 accepted as ecb9d25 with all 16 required checks green | Preserve shipping MCP17 and managed-process cleanup receipts |
| Consolidation #118 and Task #119 | Existing #118 combines reviewed admission policy, Task routing, native MCP error rejection and Cloud MCP/import work | Codex: final combined review and fresh PR/main CI; close #119 only after inclusion and merge |
| Cloud-token toolkit | Reviewed source integrated; the complete original three-token production map is independently proven and preserved. The owned Agent was minted, published, rotated, privately activated and selectively revoked; fresh readback proves replacement 200, retired 401 and all original entries 200 | Codex: finish final source CI and installed-client acceptance; preserve first failed immediate checks |
| Durable client outbox #80 | Implemented and independently reviewed in the consolidation: protected journal, stable request IDs, current claim authority checks and lazy replay; disposable Redis/PostgreSQL and shipping-crate tests pass | Codex: qualify final source CI and installed offline/reconnect behavior |

## Fleet acceptance

Deployment must use one reviewed main revision, preserve the previous binaries
and configuration, and verify downloaded artifact hashes and embedded revision
for CLI, HTTP and MCP separately. A hash proves byte identity, not health.

The initial read-only discovery found ASUS and both Sparks running revision
a0dea9d; Carbon CLI reported 3007fcd. These are historical observations, not
current installation receipts. ASUS is the authority and its HTTP service runs in
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
Carbon SSH-to-Windows read-only access and PowerShell availability are verified;
native artifact acceptance and installation still require their own receipts.
The desired-state manifest is unchanged.

Cloud public health is insufficient for fleet fallback. Claims remain on-site.
Reviewed Worker source 9e307f0 is deployed with strict keep-vars and verified
source/version parity, preserving the original opaque secret and resource/settings
identities. Authenticated raw-digest/count comparison independently established
the complete original three-token map. One new owned Agent was published; the
first immediate post-put check failed with ValueError, then an independent fresh
four-entry readback passed. Rotation, private activation and targeted revocation
are complete: fresh final readback proves replacement 200 and retired 401;
the original three entries remain unchanged, each returns 200, and first failed
immediate checks are retained. No installed consumer has been changed.
This operational evidence is separate from final shipping acceptance.

Durable client outbox #80 is implemented and reviewed with real disposable
Redis/PostgreSQL replay tests; final installed offline/reconnect acceptance is
pending. Remaining original Warp handoff work includes final consolidated
PR/main CI and native shipping artifacts, fleet installation and fresh CLI/MCP
qualification, on-site sync with replay/no-echo/no-offsite and cursor persistence,
off-LAN CLI/MCP and
individual network-location checks, and Work/NUC client reconciliation.
Historical import tooling has read-only dry-run and content-bound resume repairs;
actual scoped ingestion remains unperformed. Disabled backfill is not
evidence of historical import completion. Current P1 HTTPS health and role checks
pass with operational tooling; final installed clients and other network locations
still need their own observations.

## Cleanup predicates

Fetch current refs, compare PR and deployed heads, preserve unique tip/WIP,
check operations, ignored artifacts, locks, stashes and active writers, then
retire redundant worktrees normally. Never force-remove a locked release tree.
The primary Windows checkout retains preserved cloud-token staging and must not
be reset or switched. Reconcile this ledger after each disposition. The effective
OpenPGP provider and shared key are verified unchanged after removing one
shadowed global executable alias; historical signing-cache causality is unknown.
