# Cloud relay efficiency and account readiness

Audit cutoff: 2026-10-14. Source baseline: `2fbca7ec8812a1158df61027a41c38eb8ad65fa6`.
Local source changes and isolated test success are separate from publication,
authenticated production storage, federation and fleet acceptance.

## Observed readiness

| Check | Fresh result | Meaning / next action |
|---|---|---|
| Public Cloudflare `/health` | HTTP 200; `agentbus-cloud@0.2.0`; `storage_ready=true` | Reachability only: source returns that flag without accessing storage. |
| Unauthenticated `/presence` | HTTP 401 | Authentication gate is present. |
| Current private client file, `/presence` | HTTP 401 | This client credential does not qualify the fallback. Preserve it pending exact production-map reconciliation. |
| Local-map operator, `/sync/stats` | HTTP 401 | Local recovery map is not evidence of the deployed token map. |
| Cached Wrangler OAuth, account listing | HTTP 403, provider code 9109; cached expiry 2025-10-28 | Current entitlement, account usage, deployed settings and deployment identity remain UNKNOWN. |
| Google account / project metadata | `davidmartel07@gmail.com` ACTIVE; `dtm-gemini-ai` ACTIVE | Does not establish billing, Cloud Build, cache-bucket permissions or ADC readiness. |
| Google billing project read | `cloudbilling.googleapis.com` SERVICE_DISABLED | No API enablement or billing change performed. |

The local recovery map contains five entries across agent, hub and operator
roles. Its contents were not emitted. No token mint, rotation, revocation,
secret replacement, billing change or Worker deployment was performed.

`wrangler.toml` disables workers.dev and preview URLs, but the custom-domain
route remains commented out. The live custom hostname therefore needs
provider readback and a reviewed configuration reconciliation before a future
publication. Preserve the opaque production token binding throughout.

## Source fixes

`POST /read/batch` formerly made one sequential Durable Object RPC per agent
and deduplicated by bare message ID, while storage deliberately allows the
same ID under different origins. A native Worker regression reproduced the
loss of one origin's broadcast message. Batch reads now use one DO RPC,
preserve `(origin_hub,id)` identity, deduplicate broadcasts across agent pages,
and retain each agent's limit, broadcast selection and timestamp ordering.
For 20 agents this reduces the route's DO calls, including its unchanged
rate-limit call, from 21 to 2 (about 90.5%). SQL queries remain per-agent:
this change reduces RPC overhead without changing page selection semantics.

The returned JSON array crosses RPC as a flow-controlled byte-stream Response,
and the Worker forwards that body with its normal JSON/security headers. A
plain aggregated RPC array would exceed the platform's 32 MiB value limit for
otherwise legal per-agent pages. Native controls showed two 80-message pages
with 225 KiB bodies passed the original route, while a plain aggregated RPC
failed at 36,911,101 serialized bytes. The streamed return preserves every
record and global timestamp ordering, encodes one record per pull, releases
consumed references and clears the retained page on cancellation. It adds no
public page limit or truncation. [RPC streaming](https://developers.cloudflare.com/workers/runtime-apis/rpc/)

Rate-limit cleanup formerly queried `window_start_ms` using only a primary
key ordered by identity first. A native SQLite control read 258 rows to remove
one expired window. A window index now keeps that operation below ten rows in
the same fixture. The exact fixed-window policy, per-identity budget, cleanup
cutoff and rejection behavior are unchanged.

Validation before tool upgrades: 39 focused tests, 37 passed / two detecting
regressions failed against original source; all 39 passed after the fixes.
TypeScript 5.9.3 type checking passed. An additional route test checks that a
multi-agent batch makes one rate-limit and one read RPC.

## Requests and cost model

The Rust sync client already reuses one reqwest client. Its default healthy
loop polls messages every two seconds and presence every approximately 15
seconds; actual scheduling and request duration can reduce this frequency.
Pushes are batched to 500, pulls to 200; empty local pushes do not send HTTP.

For an idle, continuously enabled hub, a conservative 30-day model is
`2,592,000 / 2 + 2,592,000 / 15 = 1,468,800` external requests. The current
middleware plus read makes approximately 2,937,600 DO RPCs. This excludes
message/presence writes, other clients, MCP calls, administrative probes,
pagination and retention alarms; it is a projection, not measured billing.

SQLite Durable Objects are currently available on Workers Free. Historical
absence of Workers Paid does not by itself explain a storage failure.
Free DO limits are 100,000 requests/day, five million rows read/day,
100,000 rows written/day and 5 GB total SQL storage. Paid includes one million
DO requests/month, then $0.15/million, 400,000 GB-seconds/month, then
$12.50/million GB-seconds; included SQL usage is 25 billion reads,
50 million writes and 5 GB-month. Extra SQL storage is $0.20/GB-month.
Each direct stub RPC method call is a billed session. [DO pricing](https://developers.cloudflare.com/durable-objects/platform/pricing/)

At the projected polling cadence, one hub alone approaches 97,920 DO requests
per day, nearly the entire Free DO request allowance. The binding is one
global BusLog, so additional enabled hubs share that quota and storage.
The individual SQLite object limit is 1 GB Free / 10 GB Paid. [DO limits](https://developers.cloudflare.com/durable-objects/platform/limits/)

Workers Paid starts at $5/month and includes ten million Worker requests and
30 million CPU milliseconds. Additional usage is $0.30/million requests and
$0.02/million CPU milliseconds. [Workers pricing](https://developers.cloudflare.com/workers/platform/pricing/)
Under otherwise included CPU, duration and SQL usage, this one-hub model
would be approximately $5.29/month on Paid; other account workloads consume
the same allowances. Account entitlement and actual consumption must be
read back before using this estimate for a purchase decision.

## Prioritized work

| Priority | Status | Work and acceptance evidence | Owner |
|---|---|---|---|
| P0 | OPEN | Obtain authorized account metadata and exact deployed-map evidence; prove authenticated read/storage, deployment revision and current quotas without emitting credentials. | Cloud operator |
| P0 | SOURCE_VALIDATED | Integrate/review the batch identity/RPC and cleanup-index changes; run current-head CI and publish only a reviewed release. | Repository integration |
| P1 | OPEN | Replace unchecked `storage_ready=true` with distinct liveness/authenticated readiness semantics; inject DO/storage failures in native tests. Preserve existing client routing behavior deliberately. | Cloud source |
| P1 | OPEN | Adaptive idle polling, jitter and a prompt local-write wakeup; increase idle pull interval while preserving cursor, cancellation, catch-up, no-echo and presence-expiry behavior. Measure request reduction against latency. | Rust sync |
| P1 | OPEN | Measure SQL `rowsRead`/`rowsWritten` on representative retained volumes and query shapes. Optional-filter OR clauses can defeat current indexes; verify query plans before adding `(recipient,seq)` indexes or normalizing tag membership. | Cloud source |
| P1 | OPEN | Add metrics for empty pulls, requests by route/status, DO RPCs, duplicate/conflicting pushes, scanned/returned ratio, persisted cursor age and SQL bytes. Avoid logging message bodies or credentials. | Cloud operations |
| P2 | HOLD | Choose a retention window only with an archive/replay contract. Current alarm removes message/history rows but does not prune presence dedup markers, expired current presence or dependent pending acks. Bound sweeps and preserve required idempotency. | Cloud operator / source |
| P2 | OPEN | Coalesce ack/message/presence work into bounded DO operations, retaining atomicity, all-item authorization and response contracts; benchmark before changing protocol. | Cloud source |
| P2 | RESEARCH | Hibernating WebSocket invalidation/wakeup with HTTP cursor catch-up; verify reconnect, auth expiry and attachments. Keep cloud claims non-authoritative. | Transport |

For illustration, changing both idle read cadences to 60 seconds gives 86,400
external requests / 172,800 DO RPCs per 30 days, about 94% below this polling
model. It also changes latency and presence observations; it is not an applied
configuration. Prefer adaptive behavior rather than silently imposing that delay.

WebSocket hibernation keeps incoming clients connected while the DO sleeps
and stops duration charges during sleep. Ordinary long-lived SSE and outgoing
WebSockets do not establish that benefit. [Hibernation guidance](https://developers.cloudflare.com/durable-objects/best-practices/websockets/)
The Workers rate-limit binding is an alternative for edge abuse filtering,
but is intentionally permissive and eventually consistent. It cannot silently
replace this bus's exact global budget semantics. [Rate-limit accuracy](https://developers.cloudflare.com/workers/runtime-apis/bindings/rate-limit/)
Do not cache authenticated inbox/presence responses in a shared edge cache.
Conditional reads or coalescing require identity, cursor and origin scope.

## Build and cache options

Prefer the existing vigil1 runner capacity and local compiler cache first.
GitHub currently charges no Actions usage for self-hosted runners; standard
hosted runners for public repositories are also free. Artifacts, caches and
other products still have separate limits. [Actions billing](https://docs.github.com/en/billing/concepts/product-billing/github-actions)

Cloud Build's promotional default-pool e2-standard-2 allowance is 2,500
build minutes/month per billing account, then $0.006/minute, with network
egress additional. A two-vCPU cold build is not automatically faster than
vigil1's existing shared cache. Billing/API admission remains unverified.
[Cloud Build pricing](https://cloud.google.com/build/pricing)

GCS is a supported sccache backend with key prefixes and read-only/read-write
modes. Prefer workload identity/OIDC and scoped bucket access over new static
keys. [sccache GCS](https://github.com/mozilla/sccache/blob/main/docs/Gcs.md)
Its free Standard allowance includes 5 GB-month, 5,000 Class A and 50,000
Class B operations in qualifying US regions; location, operations and egress
matter. [GCS pricing](https://cloud.google.com/storage/pricing)

R2 Standard currently includes 10 GB-month, one million Class A and ten
million Class B operations/month with free direct egress. A scoped S3 cache
proof would be required, and current token permissions/account access are
unverified. [R2 pricing](https://developers.cloudflare.com/r2/pricing/)
Neither cache option should provision buckets or change retention automatically.
Compare actual cache hit rate, read latency, stored bytes and operations with
the vigil1 cache before paying for another copy.

Cloud Run worker pools are a possible elastic Linux overflow lane, but the
official GitHub-runner example also uses Cloud Build, Artifact Registry,
Secret Manager and Cloud Run as billable components. Preserve the Windows
runner lane and price the whole chain. [Runner tutorial](https://docs.cloud.google.com/run/docs/tutorials/github-runner)

## Tool version qualification

Registry checks on the audit cutoff found: Node 26.12.0 / npm 12.2.0,
Wrangler 4.151.0, Cloudflare Vitest plugin 1.4.2, workers-types
5.20261014.1, TypeScript 7.0.2, Vitest 5.0.4 and Hono 4.13.13.
The plugin declares Vitest `^4.1.0 || ^5.0.0`; Wrangler's current types peer
accepts this types release. Versions were read from the official
[Node release index](https://nodejs.org/dist/index.json) and npm registry
metadata. The isolated Node binaries were checked against official SHASUMS256.

All latest dependency peers installed cleanly, and TypeScript 7 type checking
passed. Vitest 5 failed before collecting any tests on Windows, reporting
`File path must be a file: URL` in the plugin/workerd runner. This is a visible
toolchain qualification failure; a successful Linux control alone cannot
qualify Windows. The Linux control also failed before collection, with missing
`.vitest-cache` files inside workerd. No dependencies or tests were patched to
hide those failures. Vitest remains at the latest stable 4.x release, 4.1.11,
with a visible Vitest 5 vendor-runtime HOLD.

The resulting set passed all 228 tests across 12 files on Windows and in an
owned Linux container on vigil1; TypeScript 7 type checking passed on both.
The Linux snapshot digest is
`991ab9b90e381a47f0c54442413a71eea6290c2521a0f7ab4b4c0168bba8bb0a`.
It contained 45 cloud files, including strict tool admission and current scoped edits, excluding
credentials, local configuration and node_modules. Each file was hash-checked
inside the container before compilation. The container had two CPUs, 4 GB RAM
and a 20-minute deadline; cleanup removed its exact captured ID. The Node
26.12.0 Docker image tag was not yet published, so the official checksum-checked
binary supplied the test runtime. These controls do not qualify the live Worker.

Node 26.12.0 bundles npm 11.20.0, while the current official npm release is
12.2.0. The latter was installed privately after verifying its registry SHA-512
integrity; no global npm installation changed. The package now requires Node
`>=26.12.0 <27` and npm `>=12.2.0 <13`, with project-local
`engine-strict=true`. Admission controls reject both Node 24 with npm 12 and
Node 26 with npm 11 using `EBADENGINE`, without changing the lock.

Clean npm 12 installs, TypeScript checking and all 228 tests pass on Windows
and Linux against snapshot
`be7d6e9a15e57e98cda7c9023c462f27d79fd9a3f439182967220ba9ef9a9544`.
The runtime dependency graph is unchanged from the previously qualified set.
The exact owned Linux container exited successfully and was removed. These
controls qualify the newer installer and admission metadata; they do not
establish authenticated production storage or deployment.

Context7 was attempted first but returned a monthly-quota error. Official
provider documentation and registry/release metadata supplied the fallback.
No extra paid tools or resources were enabled.
