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
| Cloud-token toolkit | Rust implementation and thin wrappers qualified on Linux, native Windows and GNU/Wine; private writes and recovery/authority gates enforced | Codex: integrate reviewed source; preserve unknown production map and obtain targeted revocation evidence separately |

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
Baseline cf7397a source-only Worker deployment is verified with no secret writes,
resource creation or settings changes; hub pull and operator stats role checks
pass. Its source receipt does not qualify this candidate's new MCP/import code.
Known hub/operator credentials permit source-only deployment and on-site sync;
the full production token map remains unknown and must not be overwritten.
Final cloud acceptance still needs source/deployment parity, off-LAN CLI/MCP
checks, federation replay/no-echo/no-offsite and the remaining original handoff
requirements. Durable locked/checkpointed outbox #80 and targeted credential
revocation remain open; a manual NDJSON spool is not their completion evidence.
Current P1 public health and authentication checks pass without an approval hold;
other network locations need their own observations.

## Cleanup predicates

Fetch current refs, compare PR and deployed heads, preserve unique tip/WIP,
check operations, ignored artifacts, locks, stashes and active writers, then
retire redundant worktrees normally. Never force-remove a locked release tree.
The primary Windows checkout retains preserved cloud-token staging and must not
be reset or switched. Reconcile this ledger after each disposition. The effective
OpenPGP provider and shared key are verified unchanged after removing one
shadowed global executable alias; historical signing-cache causality is unknown.
