# Warp fleet completion ledger

Observed 2026-10-10 UTC. Owner: Codex integration lane. This is the current
entrypoint; the fleet audit and DNS status retain their dated observations.

## Integration queue

| Item | Current evidence | Owner and next action |
| --- | --- | --- |
| Tunnel ownership #111 and timeout #112 | Both merged; installed revisions remain separate evidence | Complete installed revision; preserve final native receipts |
| Cloud dependencies #113 and #114 | Both merged; current dependencies and lockfile are accepted | Complete; preserve their original receipts |
| Fleet documentation #115 | Merged as cf7397a; stable-path migration and original hashes preserved | Source and installed deployment complete; preserve receipts |
| Inbox isolation #102 | Merged as a6bf24d after preserving and porting useful original work | Complete; disposable backend/history gates remain mandatory |
| Cloud federation #110 | Merged as d4edb4c with atomic ingestion and disposable backend regressions | Source and installed sync/restart accepted; preserve first activation failure |
| Direct routing #117 | Merged as 3b7cd9c with configured-hub routing checks | Source and installed clients accepted; preserve receipts |
| Windows GNU/Wine #120 and #121 | Both merged; #121 accepted as ecb9d25 with all 16 required checks green | Preserve shipping MCP17 and managed-process cleanup receipts |
| Consolidation #118 and Task #119 | #118 merged as 1233662bd4537bd2b2f7e1cfc034c40419ed4d21; its tree exactly matches reviewed e3e6949. Main run 38046540678 passed all 16 required checks. #119 is closed after verified inclusion; its signed tip, owned patch and ignored artifacts are preserved and its redundant worktree/branch retired | Source integration complete; preserve final receipts and ignored artifacts before normal worktree retirement |
| Cloud-token toolkit | Reviewed source integrated; the complete original three-token production map is independently proven and preserved. The owned Agent was minted, published, rotated, privately activated and selectively revoked; fresh readback proves replacement 200, retired 401 and all original entries 200 | Final main CI and both owned installed Cloud profiles pass. Preserve first failed immediate checks |
| Durable client outbox #80 | Implemented and independently reviewed in the consolidation: protected journal, stable request IDs, current claim authority checks and lazy replay; disposable Redis/PostgreSQL and shipping-crate tests pass | Final main CI and installed native offline/reconnect pass; preserve receipts |

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
readiness. Ordinary QA uses disposable or closed-port backends; installed
acceptance uses the explicitly owned nonce scope.

config/fleet/agent-bus-fleet-v1.json now binds the qualified immutable shipping
revision and records Carbon client metadata. It is desired-state documentation,
not an execution receipt. Separate actual read-only fleet doctor qualification
passes all 64 checks across ASUS, P1, Carbon and both Sparks, with no failed,
skipped or warning checks.
Keep the local Windows maintenance island out of client candidate lists.

The fleet doctor accepts both hexadecimal/date and Git-describe provenance,
comparing the complete reported revision token with ExpectedBuildRevision;
abbreviations are not treated as evidence of a full revision. It observes the
CLI's selected route without overriding routing variables and checks the hub
identity and authority. Candidate token-file checks inspect file metadata only.
An explicit token_env source still needs separate host-specific qualification.
For a local authority backend, the doctor identifies that narrower evidence;
the HTTP listener and fresh MCP process need the separate probes above.
Accepted-main artifacts from run 38046540678 have authenticated ZIP digests and
all nine binary hashes verified. The three shipping binaries are installed on
ASUS, both Sparks, P1 and Carbon with original bytes retained. P1 and Carbon
native System32 DLL loading, full revision and fresh MCP17 checks pass.
Carbon's installed CLI and dedicated MCP select the actual ASUS authority;
P1 and Carbon own actor-specific Cloud CLI/MCP health and exact synthetic
send/read/ACK pass separately. The native MCP catalog has 17 tools; the remote
Worker retains its separate eight-tool catalog. ACK origin_host is not emitted
by the deployed Cloud contract and is not inferred from native host identity.
P1 and Carbon proved both nonce/ACK directions through ASUS with the final CLI.
A separate read-only PostgreSQL transaction verifies all four exact nonce/ACK
rows against the accepted-main manifest. P1 canonical publication
preserved original security descriptors and all 22 existing process identities;
Carbon's wrapper byte update required a separate exact original ACL restoration.
Future client producers were updated without restarting existing clients.
P1 managed HTTP health also passes with the accepted shipping revision and
preserved supervisor configuration. ASUS installed sync also passes push/pull,
no duplicate or origin rewrite, a ten-second no-offsite absence window and
monotonic cursors through an observed managed restart. Source-bound disposable
backend no-echo/idempotence tests pass; live stats do not establish absence of
an outbound echo request. The first activation result failure is retained;
independent read-only recovery verified the effective service/configuration.
The desired-state manifest binds the immutable shipping revision 1233662 and
records Carbon's actual client-only topology. The remote-Windows adapter and
five-host fleet doctor pass their separate read-only qualification; native
receipts remain the proof of installed process and transport behavior.

Cloud public health is insufficient for fleet fallback. Claims remain on-site.
Deployed Worker source 0d4e4c5ed19dc49a6053873c7addea06d9aeebb5 has the exact
Worker subtree f62f71cdaaa937ade9b77cde7c352d8c5501c534 in accepted main.
Its actual deployed version remains ff191070-cf2b-40b7-a825-bd022dd18e6b;
subtree equality does not relabel that deployment as the final Git revision.
All 57 live contract checks pass, including authenticated Cloud claim-mutation
refusal and exact raw equality of the complete six-entry production token map
with the protected recovery copy. The original three entries are preserved.
The owned Agent lifecycle passed replacement 200 and retired 401 checks after
retaining the first immediate ValueError and other failed immediate checks.
The P1 and Carbon profiles use distinct Agent identities and exact host bindings,
without sharing Hub/Operator credentials. Both installed native actor-specific
Cloud profiles pass CLI health, MCP17 with semantic Cloud health, and exact
owned send/read/ACK. Cloud health is Worker0.2 and non-authoritative; it is not
relabeled as the ASUS main build. Worker-only checks do not qualify arbitrary
agents/network locations; installed sync has its separate runtime receipts.

Durable client outbox #80 is implemented and reviewed with real disposable
Redis/PostgreSQL replay tests. Installed native reconnect also passes: two
stable request UUIDs survive ten separate CLI processes and journal reopen;
reconnect applies two, a repeated flush applies zero, and exact reads return
two messages. This used an owned artificial transport interruption and did
not take the production hub offline. Accepted main passed all 16 required checks.
Its disposable Integration
Tests passed 94 tests, including no-echo/idempotence and durable recovery; the
shipping GNU/Wine suite passed 1104 tests with 16 ignored, configured MCP17, and
CLI/HTTP/PostgreSQL/SSE smoke with managed-process cleanup. These are CI proofs.
Remaining original Warp handoff work includes off-LAN CLI/MCP and individual
network-location checks, and Work/NUC client
reconciliation. Historical import tooling has read-only dry-run and content-bound
resume repairs. The first ASUS ingestion retained a failure after storing
27,259 messages and 75,261 presence events; four legacy over-length records
remain preserved on-site. Island validation retained 22 additional over-length presence records on-site and produced an exact-cell eligible copy with 7,793 events. Its first import failed with HTTP 500 before a durable checkpoint; authenticated manifest and stats reads also fail on the shared BusLog path. Server-side acceptance and provider recovery are being reconciled before any resume.
Raw LLM session histories are excluded. Disabled
backfill does not prove historical import completion. P1 and Carbon own installed
Cloud profiles pass; other network locations still need their own observations.

## Cleanup predicates

Fetch current refs, compare PR and deployed heads, preserve unique tip/WIP,
check operations, ignored artifacts, locks, stashes and active writers, then
retire redundant worktrees normally. Never force-remove a locked release tree.
Original Warp cloud-token staging must be preserved through a named scoped
stash, exact preservation refs and an independently recovered full bundle
before an ordinary fast-forward of local main and a normal branch switch.
Never reset away those preimages. Unknown ignored work and the known
`target-cloud-tokens/` build namespace remain in place; the switch must refuse
untracked or ignored path collisions. Reconcile private Git receipts after
actual disposition rather than treating this publication snapshot as cleanup.

The effective OpenPGP provider and shared key are verified unchanged after
removing one shadowed global executable alias; historical signing-cache
causality is unknown.

The Task #119 retirement preserved its full signed tip bundle, integrated owned
MCP error patch and all 4,299 ignored files (1,458,496,186 bytes). Redundant
closeout worktree retirement requires published final documentation and current
head CI, preservation of its 15 ignored cache files, and fresh exact-tip,
writer, lock and Git-operation checks. Preserve unique original branch work;
remove only redundant owned refs and worktrees through normal Git operations.
