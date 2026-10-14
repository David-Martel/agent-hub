# Fleet reconciliation and efficiency plan

Observed 2026-10-14. Coordinator: `codex-p1-agenthub-reconcile`.
Baseline source: `2fbca7ec8812a1158df61027a41c38eb8ad65fa6`.
Repository: `David-Martel/agent-hub`; local folder names may differ.

## Execution and evidence boundaries

The owner authorized parallel clone reconciliation, reviewed PR integration,
runner/build improvements and required installations. Preserve active writers,
unique commits, ignored work and private journals. Never use a live bus as a
test fixture. Source, CI, artifact identity, installed process, authenticated
runtime and Cloud synchronization each require their own evidence.

Use purpose-based artifact names. Private audit receipts and the phase ledger
live under `.Codex/state/fleet-reconciliation/`. Credentials and message bodies
must stay out of reports. An unreachable host or missing reply is not proof
that its owner is inactive.

## Current findings

- Initial local `main` matched fetched `origin/main`; the primary maintenance
  branch includes the dependency correction from PR #128 head `ea3f537`. A separate owned
  `worktrees/pr-integration` permits PR fixes while CI/cloud writers work in
  the primary checkout. The active Claude outbox worktree committed `7cc9f73`;
  preserve it and its separate review HOLD. Warp preservation remains retained.
- GitHub has six open PRs: #124, #125, #126, #127, #128 and #130. PR #128 now
  targets `main`; `ea3f537` failed configured shipping GNU MCP validation.
  run `38609697669` failed at Wine launch preflight before readiness.
  Independently reviewed batch/diagnostic head `861e7a7` is published and
  tested by run `38617998425`. All current-head gates remain required.
- git-guard 0.2.14 hygiene report/dry-run retains the active worktree. The
  open-PR cap is exceeded; do not evade it when publishing a new branch.
- Local installed CLI reads the ASUS authority, while mutation and MCP calls
  fail opening the private durable outbox. Preserve the existing journal and
  investigate custody and configuration. Remote ASUS CLI coordination works.
- Two vigil1 runners are online with Linux/X64/vigil1 labels. Docker and
  Windows cross-build admission are not established. Adding labels without
  immutable image, fixture namespace and compiler-cache proof is insufficient.
- ASUS Cloud sync reports an HTTP 500 push failure and pending outbound work.
  Public health does not establish authenticated storage readiness.
- Context7 documentation access currently reports a monthly quota limit;
  use official vendor documentation and release APIs as the recorded fallback.

### Clone custody

The sanitized inventory receipt is
`.Codex/state/fleet-reconciliation/fleet/clone-audit.json`, SHA-256
`035D2751188E3FE6225049900F853FBA5E14F226DD1A68D882D65559516E2B11`.
It establishes the following scoped observations; it is not a deployment receipt.

| Host | Observed source/runtime | Work to preserve or resolve |
| --- | --- | --- |
| ASUS | Primary and live hub at `2fbca7e`; storage ready | Artifact worktree `b903d2f`, quinn worktree `ffb486d`, local-only diagnostic `6593368`; Cloud pushes fail HTTP 500 |
| Carbon | Clean primary and Windows clients at `2fbca7e`; ASUS health passes | Detached review `99069bf` has a modified Dependabot workflow and an untracked admission test; active Codex/Claude |
| Super-NUC | Clean primary and clients at `2fbca7e`; ASUS health through owned forward passes | Active native MCP and forward task; ignored build artifacts |
| Spark 0060 | Clean primary and clients at `2fbca7e` | Detached release `2525c4d`, ignored target, active runners/builds and owned vigil1 forward |
| Spark 3066 | Clean primary and clients at `2fbca7e` | Detached release `2525c4d`, ignored target and active runners/builds |
| vigil1 | Clients at `2fbca7e`; two Linux runners online | No clone found in the scoped `~/dev` depth-five search; runner checkout paths are separate; images need toolchain admission |
| DTM-WORK | Configured SSH endpoint timed out | Source, owner, binary and configuration state remain unknown |

Carbon and NUC Windows binary hashes match; both Sparks' ARM64 hashes match.
Authenticated fresh MCP and scoped nonce tests remain separate. No redundant
worktree was removed by this audit. The desired-state fleet manifest still
names `1233662`, which must be reconciled with accepted installed revisions.

### Integration and runtime findings

PR #128 was independently replayed on native Windows: all 11 maintained MCP
smoke regression tests pass. Its latest lockfile correction matches #125 and
passes `cargo audit` without vulnerabilities. Failed run `38601341865`
truncated the first GNU MCP failure and removed its fixture. Signed head
`0a13381fb7de953d1f520d8f48ed5e02e565c2b7` now preserves the first exception,
retains failed fixtures and wraps full diagnostics; three additional independent
Linux controls pass. Current-head run `38609697669` has passed Linux, ARM64,
backend and unit gates; Windows failed before Wine launch readiness. The shipping
guest's MCP protocol and EOF behavior are not established by that failure. Duplicate
#125/#127 disposition follows accepted integration, never a check override.

PR #130 remains HOLD: its scale setting changes behavior budgets as well as
startup allowance. Independent review confirms PR #126 exposes a repository
secret to PR-controlled action code and preserves absolute archive prefixes;
29 Linux fixture tests pass without detecting these blockers. A new
`fleet-artifact-publish` environment now has verified selected Branch=`main`
policy. It contains no secrets; the original repository key remains preserved.
Credential migration, independently isolated trusted runner admission and
post-merge archival still require evidence. A source-level `if` alone cannot
protect a repository secret from a PR that changes its own workflow.

The local private journal has 1,983 queued records, 1,980 settled records and
three pending records, all expired presence operations. Its owner-only ACLs
pass inspection; its header scope differs from current routing. No payloads
were published and no journal mutation, replay or adoption occurred. The active
outbox-disposition source `7cc9f73` passed 21 focused core tests in a byte-bound
snapshot, with nine backend controls ignored. It remains HOLD: inspection drops
custody before rename, concurrent rotators can overwrite retired evidence on
Unix, and local expiry omits ambiguous attempted-request retention. Its CLI
changes were not compiled by that core-only selection. The fresh Claude commit
proves an active writer; no takeover, journal rotation or deletion occurred.

Cloud batch regressions reproduce two original-source failures and pass 39/39
after the bounded fix. A 20-agent batch uses two Durable Object RPCs including
rate limiting, down from 21; distinct `(origin_hub, id)` values remain distinct.
The rate-limit cleanup query now has a window index. These are source/test
results, not production deployment or quota-restoration evidence. Independent
review detected that a plain combined RPC array fails on a valid 36.9 MB batch.
The corrected DO returns a byte-stream Response, preserving complete pages,
deduplication and ordering; the native large-payload control passes. The latest
compatible package set passes 228 tests on both Windows and Linux and TypeScript
7 checking. A later test-only minute-boundary correction passes nine focused
controls. Signed local source commits `fd9848b` and `0d28f60` include the batch
fix and npm 12.2 enforcement. Their final clean-install source packet passes
all 228 tests and TypeScript 7 on both platforms.

The latest Cloud package upgrade exposes a native Windows Vitest 5/plugin
startup incompatibility before test collection, and Linux independently fails
with the same latest package set. Vitest 5.0.4 remains HOLD; qualified Vitest
4.1.11 is retained with explicit evidence rather than a compatibility shim.
The local Cloud actor token probes return 401; account
inspection is unavailable with the expired cached OAuth. gcloud reports the
requested account active and project `dtm-gemini-ai`, but the Cloud Billing API
is disabled and no billing/resource enablement was performed.

Rust 1.99 workspace/all-target Clippy passes after its atomic-method rename and
compiler-guided assertion diagnostics migration. Atomic orderings and closures
are unchanged; tests retain their predicates. The first isolated workspace run
stopped at routing coverage: 27 passed and nine failed in that 36-test target.
The same target then passed all 36 default-parallel selectors with two Tokio
workers per CLI child, without changing probe or transaction deadlines. The
fixture now sets that child capacity explicitly; the complete canonical run
must pass on the frozen source before a full Rust QA claim. CargoTools
environment validation passed, but its argument parser consumes native Clippy
`--fix`; the verified pinned Cargo executable was used for that operation.

The first signed tools overlay, `db9ad11`, failed actual installation because
Rustup selects installer mode by executable basename. Corrective signed source
`6d88107703c051f068301c701d5f77339c280850` retains `rustup-init`, with Dockerfile
SHA-256 `0264d993571bed615d330734b4c12a0947f5e8af47955d3006a663a1fd4497cd`.
The private Buildx 0.38/BuildKit 0.34 builder proves actual two-CPU, four-GiB
cgroups on CPUs 8-9, including a no-network build step. Its OCI import verifies
the admitted base manifest `78b794eb` and referenced blobs, with config digest
recorded separately. Immutable candidate
`sha256:6c7afb35b1df3e27c333a86560c54371bf876eec0269b5581435d282fe2f513b`
has matching source labels and entrypoint/root contract. Its native/Windows
cache control proves four requests, two Rust hits, zero errors and identical
artifacts; a linked Windows x64 console program runs under Wine. This does not
qualify the full agent-hub artifacts or admit a runner. Its inherited GCC 13
is below the [exact Rust 1.99 Windows GNU baseline](https://raw.githubusercontent.com/rust-lang/rust/1.99.0/src/doc/rustc/src/platform-support/windows-gnu.md)
of GCC 14.2. Latest GCC 16.2 and LLVM-MinGW alternatives require a separately
qualified compiler/CRT dependency closure. All owned probe children were removed
after exit/PID-zero checks; the private builder is stopped and its cache retained.

The independently reviewed Wine adapter now batches four path helpers into two
while preserving each round-trip proof and guest configuration readback. The
measured preflight drops from 31.98 to 22.33 seconds; initialize, 17 tools and
settlement pass within unchanged deadlines. This replay uses the prior verified
`f2c64a18` binary with the modified adapter and cannot qualify a current-head
binary. Safe exclusive mode-0600 failure receipts retain only static code
positions, exception classes and stage timings. The original CI early EOF is
not claimed to share that measured timeout cause. Signed PR #128 commit
`861e7a700f677389d338de54d683d9c9824daece` carries the fix; current-head CI remains
required before merge or duplicate PR disposition.

## Work queue

| ID | Work | Owner | Status and completion evidence |
| --- | --- | --- | --- |
| F1 | Inventory every reachable clone, registered worktree, owner, service and binary/config identity | fleet audit | SCOPED_AUDIT_COMPLETE; sanitized host receipt; DTM-WORK remains unreachable |
| F2 | Review PR stack, resolve duplicate dependency work, repair first failing checks and integrate current heads | coordinator / CI audit | IN_PROGRESS; 16 required checks, review and exact merge SHA |
| F3 | Bind vigil1 Linux scheduling and qualify Docker/Windows build lanes | CI audit | IN_PROGRESS; actual runner labels, owned fixtures, immutable image and smoke/cache receipts |
| F4 | Verify latest stable tool releases and update coordinated compiler/image/cache pins | CI audit | OPEN_COMPILER_FLOOR; built tools overlay/cache/x64 console controls pass, GCC 13 remains below Rust 1.99 baseline; fleet engines unchanged |
| F5 | Reduce Cloudflare request/SQL work and preserve batch identity | cloud audit | SOURCE_VERIFIED / DEPLOYMENT_BLOCKED; signed fixes, 228 tests on each platform, account access unresolved |
| F6 | Repair local outbox/config custody without losing historical pending requests | coordinator | OPEN; exact authority reconciliation and fresh native CLI/MCP mutations |
| F7 | Install accepted artifacts/configuration only where required; qualify each installed process | fleet / coordinator | OPEN; preserved prior bytes, hashes, embedded revisions, fresh CLI/MCP and scoped nonce |
| F8 | Retire proven redundant PRs, branches and worktrees | coordinator | OPEN; fresh writer checks, preservation and exact-tip deletion guards |
| F9 | Compare Cloudflare/GitHub/gcloud cost and capacity options | cloud audit | RESEARCH_COMPLETE / ACCOUNT_VALIDATION_OPEN; official prices, measured fixture budget and explicit access gaps in cloud-efficiency.md |

## Tool and skill routing

Use phase-runner for explicit dependencies and failure receipts, cargo-build
for Windows CargoTools/environment validation, ci-triage for exact failed job
logs, cloudflare-deploy for Worker/Durable Object tooling, and Context7 for
current API documentation when available. Commit-cluster, context toolkit and
rigorous verification apply at publication and handoff. CLI/SSH provide a
working coordination alternative to the currently failing local MCP outbox.
HTTP/SSE and MCP acceptance remain distinct transport checks.

Batch independent read-only queries and return concise findings from agents.
Retain exact raw output for hashes, Git identities, counts and test receipts;
lossy RTK compression must not be used for those evidence checks.
