# agentbus.dtmventures.com — off-site Cloudflare tier (agent-hub#79/#82)

A Cloudflare Worker that implements the subset of the on-site agent-bus HTTP
API listed in agent-hub#79 (send / read / inbox / presence / claims / ack /
health), plus the `/sync/push`, `/sync/pull`, `/sync/push-presence` and
`/sync/stats` hub<->cloud replication endpoints (agent-hub#79/#82), so the
existing `agent-bus` CLI/MCP work unmodified against
`server_url=https://agentbus.dtmventures.com`.

**agent-hub#82** hardened the auth model (every actor field is now bound to
the caller's bearer-token identity — see "Auth model" below) after a
pre-deploy security review found identity spoofing, an unauthenticated
claims authority, and validation gaps on `/sync/push`. Read that section
before deploying or issuing tokens.

D1 and R2 are **not used**: the Cloudflare API token available for this build
denies both (error 10000). Every table lives in Durable Object **SQLite
storage** (`ctx.storage.sql`) instead — see "Design change" below.

## Auth model (agent-hub#82)

Every bearer token in `AGENT_BUS_TOKENS` maps to `{agent, host?, role, hub?}`
— see `src/auth.ts`'s docblock for the full model. Three roles:

- **`agent`** — bound to that exact agent/host identity. A body/path value
  (`sender`, ack/knock `agent`, claim/renew/release `agent`, presence `:agent`)
  that DISAGREES with the token's own identity is a `403`; a matching or
  omitted value resolves to the token's identity.
- **`hub`** — may VOUCH for any agent value (needed so the on-site hub can
  relay real on-site agents' actions through one token — see
  SYNC-CONTRACT.md §6). Required for `/sync/push`, `/sync/push-presence`, and
  (together with `operator`) `/sync/pull`. Its `hub` field is the ONLY source
  of `origin_hub` on every `/sync/*` route — never the request body.
  Malformed entries (missing `hub` for a `hub`-role token) are rejected at
  token-map parse time, fail closed.
- **`operator`** — may also vouch for any agent value. Required for
  `PUT .../resolve` (the global claims authority's arbitration decision) and
  `GET /sync/stats`.

The `AGENT_BUS_AUTH_TOKEN` shared-token fallback is disabled unless
`AGENT_BUS_DEV_ALLOW_SHARED_TOKEN=1` is ALSO set, and always resolves to role
`agent` — it can never reach `/sync/*` or `resolve` even when enabled. Not
recommended for production; configure `AGENT_BUS_TOKENS` instead.

### `AGENT_BUS_TOKENS` format, exactly (agent-hub#82 re-review N1)

`AGENT_BUS_TOKENS` MUST be a JSON **object** whose keys are the bearer tokens
themselves — never a JSON array. `parseTokenMap` (`src/auth.ts`) fails
CLOSED on the whole secret if the top-level value is an array, `null`, or
any other non-object shape: every token then gets `401`, rather than the
array-index bug this closes (a secret accidentally shaped as `[{"agent":
"op","role":"operator",...}]` made the literal `Bearer 0` authenticate as
operator, because `Object.entries()` on an array yields index keys). Per
entry: unknown keys are rejected (the whole entry is dropped, not just the
unknown field), and the token itself must be **at least 32 characters** —
both checks fail closed on that one entry, not the whole map.

Worked example (placeholder tokens below are obviously fake — never real
credentials; a real token is a long random string from your secrets
manager, not a readable phrase like these):

```json
{
  "0000000000000000000000000000dead": { "agent": "claude", "host": "asuspro13", "role": "agent" },
  "1111111111111111111111111111beef": { "agent": "codex", "host": "asuspro13", "role": "agent" },
  "2222222222222222222222222222cafe": { "agent": "asuspro13-sync", "role": "hub", "hub": "asuspro13" },
  "3333333333333333333333333333f00d": { "agent": "operator", "role": "operator" }
}
```

Set it with `wrangler secret put AGENT_BUS_TOKENS` and paste the (real,
never-shared) JSON on stdin — never commit it, and never pass it inline to
any MCP config or shell history.

`Identity.token` (the raw bearer, previously stashed "for audit logging") has
been removed — nothing ever logged it, and doing so would leak a live token.

### Cloud tokens are their own credential space — NEVER the on-site hub's (review L5)

**Every token in this Worker's `AGENT_BUS_TOKENS` MUST be generated fresh,
specifically for the cloud tier, and MUST NEVER be the same value as the
on-site agent-bus hub's `AGENT_BUS_AUTH_TOKEN`** (the token asuspro13, both
DGX Sparks and dtm-p1gen7 authenticate to each other with — see the main
repo's `~/.config/agent-bus/config.json` / `hub.env`, most recently rotated
2026-09-27). These are two separate trust domains with different blast
radii: the on-site hub token is trusted by every fleet host on the LAN/p2p
fabric, while a cloud token is reachable from the public internet. Reusing
one token across both means a single leak (a misconfigured client, a log
line, a compromised laptop) compromises BOTH tiers simultaneously, and
rotating one tier's token silently breaks the other's auth instead of being
an isolated, low-drama rotation.

Concretely:
- Generate the cloud tier's `AGENT_BUS_TOKENS` entries with their own random
  values (e.g. `openssl rand -hex 32`), independent of any on-site secret.
- The future hub→cloud sync client (the on-site `asuspro13` process that
  will call `POST /sync/push` / `/sync/push-presence` against this Worker —
  see SYNC-CONTRACT.md) authenticates with its OWN dedicated hub-role cloud
  token, stored separately from `hub.env` (which holds the on-site hub's own
  token, for on-site fleet auth, not cloud auth). Never point that client at
  the on-site hub token.
- `AGENT_BUS_AUTH_TOKEN` (the shared, unbound, lowest-privilege dev fallback
  — see above) must never be set to a real deployed secret at all, on-site
  hub token or otherwise; it exists only for local `vitest`/`wrangler dev`
  runs. `GET /health` surfaces a `warnings` array naming this exact
  misconfiguration when `AGENT_BUS_DEV_ALLOW_SHARED_TOKEN=1` is set, so a
  post-deploy smoke check (or a human) can catch it without grepping Worker
  logs — see `test/health-auth.test.ts` for the regression test.
- Storing and generating the real values is a deploy-time operational step,
  not something this repository does: whoever runs `wrangler secret put`
  generates fresh tokens and keeps them 0600 outside the repo (e.g. in a
  password manager or a `0600` file under `~/.config/`), never committed
  here.

## Route table

Derived directly from `crates/agent-bus-http/src/http.rs`'s router
(`start_http_server`) and the `agent-bus-core::ops` / `channels` modules it
delegates to. "Rust source" names the function(s) this route mirrors. The
"Auth" column names the role required beyond a valid bearer token; "bearer"
alone means any role.

| Method | Path | Auth | Rust source | Request body | Response |
|---|---|---|---|---|---|
| GET | `/health` | open | `http_health_handler` (redacted subset) | — | `Health` (no fleet counts) |
| GET | `/admin/tokens/manifest` | **operator** | cloud-only read-only provenance | — | exact UTF-8 secret-binding SHA-256, unfiltered entry count, existing build/hub identity |
| POST | `/messages` | bearer (sender bound to identity) | `http_send_handler` -> `validated_post_message` | sender, recipient, topic, body, thread_id?, tags?, priority?, request_ack?, reply_to?, metadata?, schema?, +#79: client_msg_id?, origin_host?, origin_hub?, hlc?, sensitivity? | `Message` |
| GET | `/messages` | bearer | `http_read_handler` -> `list_messages_live` | query: agent, from, topic, repo, session, tag*, thread_id, since, limit, broadcast, excerpt | `Message[]` |
| POST | `/messages/batch` | bearer (sender bound per item) | `http_batch_send_handler` -> `validated_batch_send` | `{messages: SendBody[]}` (<=100) | `{ids, count}` |
| POST | `/messages/:id/ack` | bearer (agent bound — "ack only for yourself") | `http_ack_handler` -> `post_ack` | `{agent, body?}` | `{ack_sent, ack_message_id, acked_message_id, timestamp}` |
| POST | `/read/batch` | bearer | `http_batch_read_handler` -> `bus_list_messages_from_redis` | `{agents[], since?, limit?, broadcast?}` (<=20 agents) | `Message[]` (deduped, sorted) |
| POST | `/ack/batch` | bearer (agent bound) | `http_batch_ack_handler` | `{agent, message_ids[], body?}` (<=100) | `{acked, message_ids}` |
| POST | `/knock` | bearer (sender bound) | `http_knock_handler` | `{sender, recipient, body?, thread_id?, tags?, request_ack?}` | `Message` (topic=knock, priority=urgent) |
| PUT | `/presence/:agent` | bearer (agent bound) | `http_presence_set_handler` -> `set_presence` | `{status?, session_id?, capabilities?, ttl_seconds?, metadata?}` +#79 `network_context?` | `Presence` |
| GET | `/presence` | bearer | `http_presence_list_handler` -> `ops_list_presence` | — | `Presence[]` (live only) |
| GET | `/presence/history` | bearer | `http_presence_history_handler` -> `list_presence_history_postgres` | query: agent, since, limit | `Presence[]` |
| GET | `/notifications/:agent_id` | bearer | `http_notifications_handler` -> `list_notifications[_since_id]` (approximated — see below) | query: since_id, history | `Notification[]` |
| GET | `/pending-acks` | bearer | `http_pending_acks_handler` -> `ops_list_pending_acks` | query: agent | `PendingAck[]` |
| POST | `/channels/arbitrate/:resource` | bearer (agent bound) | `http_claim_handler` -> `claim_resource_with_options` | `{agent, priority_argument?, mode?, namespace?, scope_kind?, scope_path?, repo_scopes?, thread_id?, lease_ttl_seconds?, scope?}` | `OwnershipClaim` |
| GET | `/channels/arbitrate/:resource` | bearer | `http_arbitration_state_handler` -> `get_arbitration_state` | — | `ArbitrationState` |
| PUT | `/channels/arbitrate/:resource/resolve` | **operator** | `http_resolve_handler` -> `resolve_claim` | `{winner, reason?, resolved_by?}` | `ArbitrationState` |
| POST | `/channels/arbitrate/:resource/renew` | bearer (agent bound — owner-only) | `http_renew_claim_handler` -> `renew_claim` | `{agent, lease_ttl_seconds?}` (TTL capped 1..86400s) | `OwnershipClaim` |
| POST | `/channels/arbitrate/:resource/release` | bearer (agent bound — owner-only) | `http_release_claim_handler` -> `release_claim` | `{agent}` | `ArbitrationState` |
| GET | `/resource-events/:resource_id` | bearer | `http_resource_events_handler` (bonus — cheap given ClaimDO already has the data) | query: limit | `ResourceEvent[]` |
| POST | `/sync/push` | **hub** | new, agent-hub#79/#82 | `{origin_hub?, messages: SyncPushMessageInput[]}` (<=500; `origin_hub` always comes from the token, not the body) | `{accepted[], duplicates[], conflicts[], rejected[], cursor}` |
| GET | `/sync/pull` | **hub or operator** | new, agent-hub#79/#82 | query: since, exclude_origin, limit | `{messages[], next_cursor, has_more}` |
| POST | `/sync/push-presence` | **hub** | new, agent-hub#82 | `{origin_hub?, origin_host?, events: [...]}` (<=500) | `{accepted: int, duplicates: int, rejected: [...]}` |
| GET | `/sync/stats` | **operator** | new, agent-hub#82 | — | `{messages: [{origin_hub, count}], presence: [{origin_hub, count}]}` |

### Deferred (out of the agent-hub#79 cloud subset — return `501`, not `404`)

`/events*` (SSE),
`/admin/*` (except `/admin/tokens/manifest`), `/channels/direct/*`, `/channels/groups*`, `/channels/escalate`,
`/channels/summary`, `/token-count`, `/compact-context`, `/session-summary`,
`/thread-summary`, `/compact-thread`, `/orchestrator-summary`, `/tasks/*`,
`/subscriptions*`, `/inventory`, `/threads*`, `/overdue-acks`, `/dashboard*`,
`/support`. These match issue #79's own scope table (`send/read/inbox/
presence/claims/ack/health`) — nothing here was silently dropped.

### One documented approximation: `/notifications/:agent_id`

The Rust hub fans every send out to a dedicated per-recipient Redis stream
(`agent_bus:notify:<agent>`) with its own `reason`/`requires_ack` bookkeeping.
The cloud tier derives the same `Notification[]` shape directly from the
message log (`to = agent OR to = 'all'`) instead of maintaining a second
write path. A request without `since_id` takes a newest-first snapshot.
With `since_id` (including `0-0`), it returns oldest-unread-first pages;
advance to the last returned `id` to drain pages without skipping unread
rows. `id`/`notification_stream_id` are the message's own `seq`, not an
independent notification-stream id. The cloud does not implement the native
MCP `check_inbox` tool's persistent, filter-scoped cursors.

### MCP over authenticated HTTP

`POST /mcp` accepts one JSON-RPC 2.0 request per JSON body. It implements
`initialize`, `ping`, `tools/list` and `tools/call`; initialized/cancelled
notifications return empty `202` responses. This is a stateless JSON
transport, with no SSE, sessions or server callbacks. `GET /mcp` returns
`405`. Protocol negotiation supports `2024-11-05`, `2025-03-26`,
`2025-06-18` and `2025-11-25`; it does not advertise the newer handshake-free
protocol. Existing native `HttpMcpTransport` direct `tools/call` requests
remain usable without a prior handshake or additional version headers.

The catalog contains exactly eight native-named tools: `bus_health`,
`post_message`, `list_messages`, `ack_message`, `set_presence`,
`list_presence`, `list_presence_history` and `knock_agent`. Argument names,
types, bounds and required fields match their native schemas. Results are
JSON serialized inside `result.content[0].text`, as the dedicated native
MCP client expects. These operations use the same REST handlers, validation,
sender/agent binding and recipient-only ack checks. Agent tokens cannot
impersonate another agent; hub/operator tokens retain existing vouching
rights. Each external RPC consumes one existing per-identity rate-limit
unit; internal REST dispatch re-authenticates the same bearer.

Claims, arbitration, local channels, native inbox cursors and `negotiate`
are not advertised or emulated. Cloud health describes cloud SQLite
storage, not Redis/PostgreSQL or on-site authoritative readiness. A knock
is durable but does not promise cloud SSE delivery.

Use a cloud-only identity-bound bearer stored privately; never reuse an
on-site hub token or put it in command arguments, logs or committed MCP
configuration. Send `Content-Type: application/json`; ordinary Streamable
HTTP clients should send `Accept: application/json, text/event-stream`.
Browser Origins must match the request origin. Requests are bounded to
1 MiB while reading, including requests without `Content-Length`.
Malformed envelopes/methods/arguments receive JSON-RPC errors. Existing REST
authorization or validation failures become `isError: true` tool results
with a generic rejection/status; internals and rejected payloads are not
echoed. This source change does not activate a fleet cloud candidate or
change the on-site claims authority. An absent `MCP-Protocol-Version` header
uses the compatible `2025-03-26` transport rules; an explicit unsupported
header is rejected with HTTP `400`.

### Read-only token-map provenance

An operator-role bearer may read `GET /admin/tokens/manifest`. It returns
`representation: "utf8-secret-binding-v1"`, SHA-256 of the exact UTF-8
`AGENT_BUS_TOKENS` binding string, and `entry_count` for every top-level
object key, including malformed entries the authentication parser filters
out. Whitespace, Unicode and trailing newlines affect the digest; it is
never computed from a reserialized or filtered map. Malformed/non-object
bindings fail closed. No token entries, individual digests, roles or secret
values are returned or logged. Agent and hub roles are denied.

The response also includes the existing `build_version` and `hub_identity`;
the static build label is not an immutable deployment ID or Git revision.
Bind the response to separately authenticated Cloudflare deployment/source
evidence before accepting a preserved map. Compare the complete candidate
bytes/hash/count locally after strict parsing; do not infer map equivalence
from one usable token. This endpoint cannot modify credentials, and does
not authorize a replacement or rotation. All responses use `no-store`.

## Schema additions (agent-hub#79/#82, additive & optional)

`Message` gains `client_msg_id` (UUIDv7), `origin_host`, `origin_hub`,
`origin_seq`, `hlc`, `sync_state` (client-side only), `sensitivity`
(`"internal"` default | `"no-offsite"`). `Presence` gains `network_context`
and (agent-hub#82) `origin_hub` — set only for presence relayed via
`/sync/push-presence`, mirroring `Message.origin_hub`; absent for presence
set directly via `PUT /presence/:agent`. None of these change any existing
field's name, type, or omission rule — see `src/types.ts` for the full
mapping and `crates/agent-bus-core/tests/cloud_fixtures.rs` for the Rust-side
fixtures this Worker is tested against. Neither Rust struct declares
`#[serde(deny_unknown_fields)]`, so an additive cloud-only field is silently
ignored by a Rust deserializer that doesn't know about it yet — verified by
grep before adding `origin_hub` to `Presence`.

`sensitivity: "no-offsite"` messages are rejected by both `POST /messages`
and `POST /sync/push` — the latter FAIL CLOSED on any value other than
`internal`/`no-offsite` (agent-hub#82 review H4: an exact-string comparison
previously let `No-Offsite`/`no_offsite`/`"no-offsite "` all through
verbatim). A best-effort PHI pattern screen (`containsObviousPhi` in
`src/validation.ts`) also rejects obvious SSN/MRN/DOB/patient-identifier
patterns on every message-shaped body, on `/knock` and ack bodies, and on
metadata (message and presence) — defense in depth, not a substitute for
`sensitivity`.

## Security hardening (agent-hub#82)

Beyond the identity/role model above:

- **Validation parity.** `/sync/push` runs the SAME core validator as
  `/messages` (`validateMessageCore` in `src/validation.ts`) — length/NUL
  caps, priority, a `tags: string[]` type check, the fail-closed sensitivity
  check, metadata/total-size caps, and the PHI screen — with ONE deliberate
  exception: schema auto-fit/validation (which *mutates* the body, e.g.
  prepending `FINDING:`/`SEVERITY:`) is NOT applied to synced historical
  imports, only to direct `POST /messages`. See SYNC-CONTRACT.md.
- **Per-origin dedup.** `messages` is keyed `UNIQUE(origin_hub, id)` /
  `UNIQUE(origin_hub, client_msg_id)`, not a global unique key — a
  cross-origin id collision is reported as a `duplicate` (same content) or a
  `conflict` (different content) rather than one origin silently pre-empting
  another's write.
- **Caps.** Metadata capped at 64 KB, body at 256 KB (unchanged), a
  defense-in-depth 400 KB total-message cap, tags capped at 64 entries of
  256 chars each, claim/renew lease TTL capped to 1..86400 seconds.
- **Every remaining field is type-checked and length-capped (re-review
  N2/N3/N4).** `sender`/`recipient`/`topic` (256), `thread_id` (256),
  `reply_to`/`client_msg_id`/`hlc` (128), `origin_host`/`origin_hub` (256) on
  messages; `status` (64), `session_id` (256), `capabilities` (`string[]`,
  same caps as `tags`) and a validated `network_context` enum on presence
  (both `PUT /presence/:agent` and `POST /sync/push-presence`);
  `priority_argument` (512), `namespace`/`scope_kind` (256/128),
  `scope_path` (1024) and `repo_scopes` (`string[]`) on claims. The sum of
  every field at its individual cap, plus the 400 KB total-size backstop,
  both stay well under half of Cloudflare's documented ~2 MB per-row limit
  for Durable Object SQLite storage (`DO_SQLITE_ROW_LIMIT_BYTES` in
  `src/validation.ts`; `test/security.test.ts` asserts the arithmetic so a
  future cap increase that erodes the margin fails loudly).
- **PHI screen extended to `topic`, `thread_id`, `recipient` and presence
  `status`** (re-review M7/N2/N3) — previously only the body, ack/knock body
  and metadata were screened.
- **Bounded tag scan.** `GET /messages?tag=...` examines at most 10,000 rows
  per call (`MAX_TAG_SCAN_ROWS` in `src/do-buslog.ts`) rather than paging
  through the entire 7-day window when a tag filter matches nothing.
- **Retention.** Off (unlimited) by default. Set `RETENTION_DAYS` (a Worker
  var, e.g. `RETENTION_DAYS = "90"` in `wrangler.toml`'s `[vars]`) to enable a
  daily `BusLog` Durable Object alarm that deletes `messages`/
  `presence_history` rows older than that window. Long or off by default is
  intentional — the operator decides the actual window.
- **Rate limiting.** A fixed-window (60s) per-identity counter lives in the
  `BusLog` Durable Object (`checkRateLimit`, keyed on `role:agent:host` —
  NEVER a token or token hash), applied to every authenticated route in the
  auth middleware. Default 600 requests/minute per identity, overridable via
  the `RATE_LIMIT_PER_MINUTE` Worker var. **Upgrade path**: if the deploying
  Cloudflare account has a Workers rate-limiting binding available
  (`unsafe_hello_world`-style `[[unsafe.bindings]]` with `type =
  "ratelimit"`), prefer that at the edge instead — it was not used here
  because availability could not be verified against the account this build
  targets without touching production.
- **No ClaimDO minted on a read.** `GET /channels/arbitrate/:resource` and
  `GET /resource-events/:resource_id` on a resource nobody has ever claimed
  return an empty result without ever running `CREATE TABLE` against that
  resource's Durable Object storage.
- **Resource-name normalization.** Claim resource names are folded
  (backslash -> forward slash, lowercased, leading `./` stripped, capped at
  256 chars) via `normalizeResourceName` in `src/claims-logic.ts`. The Rust hub
  retains its separator-only normalization for on-site claim and event keys,
  preserving case, leading `./`, existing leases and history. The two tiers
  keep separate claim state: the hub does not proxy claims to the cloud
  (see `docs/cloud-sync.md`).
- **Generic 500s.** The error handler (`guarded()` in `src/index.ts`) never
  echoes a raw SQLite error, a `Date` parsing failure, or any other internal
  exception message to the caller — every non-`ValidationError`/
  `ForbiddenError` failure returns a fixed `{"error":"internal error"}` body.
- **Response headers.** Every JSON response (including 401/403/404/500) sets
  `cache-control: no-store` and `x-content-type-options: nosniff`.
- **`workers_dev = false` / `preview_urls = false`** in `wrangler.toml` — the
  Worker is reachable ONLY via the configured custom domain route, never a
  `*.workers.dev` URL, once deployed (see the deploy checklist's note on the
  one-time smoke-test exception).

## Design change from the original brief (operator-approved, 2026-09-27)

D1 and R2 are DENIED on the available dtmventures.com API token (error
10000). Per operator direction:

1. Durable Objects with the **SQLite storage backend** (`new_sqlite_classes`
   migration, `ctx.storage.sql`) are the PRIMARY store — `BusLog` (messages,
   presence, pending-acks, sync cursors; single instance at current ~27k-
   message volume) and `ClaimDO` (one per resource, via `idFromName`).
2. No D1 anywhere in this build.
3. R2 snapshots are optional/deferred — `wrangler.toml` has the binding
   commented out; nothing here requires it to deploy.
4. Tests run under `@cloudflare/vitest-plugin` + Miniflare, which supports
   SQLite-backed Durable Objects directly (see "Tests" below — this is also
   the current package name: `@cloudflare/vitest-pool-workers` was renamed to
   `@cloudflare/vitest-plugin` on 2026-08-19).
5. No D1/R2 permission is required to deploy this Worker.

## Tests

```bash
npm ci
npx tsc --noEmit
npx vitest run
```

9 test files, 130+ tests, all passing (grows as regressions are added):

- `test/health-auth.test.ts` — `/health` open + reveals no fleet data; auth
  required on every other route; deferred routes return 501.
- `test/messages.test.ts` — send/read/batch-send/ack/read-batch/ack-batch/
  knock; schema auto-fit (`finding`/`benchmark`); priority validation;
  `sensitivity: "no-offsite"` rejection; PHI-pattern rejection;
  `client_msg_id` round-trip; broadcast/topic/tag filters; identity binding
  (a mismatched `sender`/ack `agent` is 403; an agent-role token's own
  identity is filled in when omitted).
- `test/presence.test.ts` — set/list/history, `network_context`,
  `(key_origin, agent)` collision avoidance across two hubs, presence
  metadata PHI screening, agent-identity binding on `PUT /presence/:agent`.
- `test/inbox.test.ts` — `/notifications/:agent_id` (incl. `since_id`
  pagination and `requires_ack`), `/pending-acks` (incl. `stale`).
- `test/claims.test.ts` — claim/renew/release/resolve/get; shared vs.
  exclusive vs. shared_namespaced conflict rules; reroute suggestions
  (generic and machine-global-pattern); **exclusive-claim contention across
  two concurrent `Promise.all` requests**; lease expiry (1-second TTL, pruned
  on next read); lease TTL capping; resource-events lifecycle including "no
  ClaimDO minted on an unclaimed resource's read"; resource-name
  normalization (case-fold, backslash-fold); owner-only renew/release;
  operator-only resolve; auth.
- `test/sync.test.ts` — push/pull/push-presence/stats, all now hub- or
  operator-gated; **idempotent re-push** on `id` and on `client_msg_id`
  alone; **per-origin conflict detection** (same id, different content);
  **cross-origin non-pre-emption** (same id, different origin_hub, both
  kept); fail-closed sensitivity; per-item body/tags/PHI/size validation;
  `origin_seq` round-trip; per-item `origin_hub` mismatch rejection; schema
  auto-fit is NOT applied; historical `id`/`timestamp_utc`/`protocol_version`
  preservation with format validation; pull pagination (`next_cursor`/
  `has_more`); `exclude_origin` filtering; `push-presence` dedup on
  `(origin_hub, origin_id)`.
- `test/contract.test.ts` — the field-for-field contract test against the
  Rust-generated fixtures in `test/fixtures/*.json` (see below).
- `test/smoke.test.ts` — proves the SQLite-backed DO + Workers RPC path
  works at all under Miniflare before anything else is asserted against it.
- `test/security.test.ts` — one regression test per P-numbered exploit probe
  and H/M/L-numbered finding in the agent-hub#82 security review, named by
  finding id for direct traceability back to the review document.

### Contract fixtures

`crates/agent-bus-core/tests/cloud_fixtures.rs` (in the Rust workspace)
serializes real `Message`/`Presence`/`OwnershipClaim`/`ArbitrationState`
values with `serde_json::to_value` and writes them to `test/fixtures/*.json`
(regenerate with `REGEN_FIXTURES=1 cargo test -p agent-bus-core --test
cloud_fixtures`). `test/contract.test.ts` feeds those exact JSON shapes
through the Worker's Durable Objects and asserts field-for-field equality —
including which optional fields are *omitted* vs. present as `null` — with
regex checks only on the fields that are inherently dynamic (timestamps).

## Deploy checklist

Nothing above requires D1 or R2 access — only Workers scripts, Durable
Object namespaces, and (for the custom domain) zone DNS/Routes permission.

1. **Verify the API token** can manage Workers scripts and Durable Object
   namespaces and read the `dtmventures.com` zone (already confirmed for
   this build). Separately verify it has **zone Workers Routes / Custom
   Domains** permission before attempting step 5 — if it doesn't, deploy to
   the `workers.dev` subdomain first (see step 4) and add the custom domain
   in a follow-up once that permission is granted.
2. **Secrets** (Bitwarden item `cloudflare.com`, never committed, never
   passed inline to any MCP config):
   ```bash
   npx wrangler secret put AGENT_BUS_TOKENS      # JSON map, see .dev.vars.example
   npx wrangler secret put AGENT_BUS_AUTH_TOKEN   # optional shared-token fallback (see below)
   ```
   `AGENT_BUS_TOKENS` entries now require a `role` (agent-hub#82 — see "Auth
   model" above): at minimum, mint one `hub`-role token per on-site hub that
   will call `/sync/*` (its `hub` field is authoritative for that hub's
   `origin_hub`), and one `operator`-role token for whoever calls
   `PUT .../resolve` and `GET /sync/stats`. Do **not** set
   `AGENT_BUS_DEV_ALLOW_SHARED_TOKEN` in production — leaving it unset means
   `AGENT_BUS_AUTH_TOKEN` (if set at all) is never honored.
3. **Bindings** — already declared in `wrangler.toml`, nothing to create by
   hand: `durable_objects.bindings` (`BUS_LOG` -> `BusLog`, `CLAIM_DO` ->
   `ClaimDO`) and the `new_sqlite_classes` migration. No KV, no D1, no R2.
4. **First deploy (smoke test)**: `wrangler.toml` sets `workers_dev = false`
   (agent-hub#82 review M8; `workers_dev` is deploy-routing config, not a
   runtime var, so it cannot be overridden with `--var` — it must be edited
   in the file). Before DNS is live there is no public URL to `curl` at all
   with `workers_dev = false` and no `[[routes]]`, so verify this deploy with
   `wrangler dev --remote` (talks to the real Durable Object namespace
   without publishing a public route) or via direct DO RPC (see `test/
   smoke.test.ts`'s pattern) instead:
   ```bash
   npx wrangler deploy
   npx wrangler dev --remote   # confirms /health against the deployed Worker
   ```
   Only if a public-URL smoke test is truly needed, temporarily edit
   `workers_dev = true` in a LOCAL, uncommitted change, deploy, `curl
   https://<worker-name>.<account-subdomain>.workers.dev/health`, then revert
   the file to the committed `workers_dev = false` and redeploy before
   proceeding to step 5. Never commit `workers_dev = true`.
5. **Custom domain** — uncomment the `[[routes]]` block in `wrangler.toml`
   (`pattern = "agentbus.dtmventures.com"`, `custom_domain = true`) and
   redeploy. Per `DTMVentures/headscale-ops` policy, `agentbus` is a generic
   name — no fleet device name (asuspro13, spark-*, vigil1, dtm-p1gen7, ...)
   may ever appear in public DNS for this zone.
6. **Use a cloud-scoped client credential**. Never assign the cloud token
   to `AGENT_BUS_AUTH_TOKEN` or reuse the on-site token. Candidate-aware Rust
   clients accept a separate source and an explicit non-authoritative role:

   ```json
   {"server_urls": [
     "http://hub.internal:8400",
     {"url":"https://agentbus.dtmventures.com", "role":"cloud", "token_file":"~/.config/agentbus-cloud/agent.token"}
   ]}
   ```

   This example describes credential isolation, not fleet readiness. The
   Worker's authenticated `/mcp` endpoint supports only the eight tools
   documented above; several CLI routes remain deferred. Do not add it to the deployed fleet
   candidate list until the required routes, on-site synchronization and
   role-scoped credentials have been validated. Public `/health` success
   verifies reachability only; unauthenticated `/mcp` must return `401`.
7. **The Rust side.** The on-site hub's sync task (push, pull, presence,
   `cloud_*` health) is in `crates/agent-bus-http/src/cloud_sync.rs`; see
   `docs/cloud-sync.md` for configuration. It is off until a cloud URL, a
   0600 token file and `hub_identity` are set. The
   `/channels/arbitrate/*` proxy-to-cloud switch described in SYNC-CONTRACT.md
   §6 is NOT built, and the plan is to keep claims on-site instead; that needs
   an owner decision recorded against §6.
8. **Historical import** — the one-time backfill importer at
   `~/.local/share/jules-fleet/handoff-2026-09-26/agentbus-import/import_pg_export.py`
   targets `/sync/push` and `/sync/push-presence` exactly as implemented
   here (no importer changes needed). Its `AGENTBUS_TOKEN` must be a
   `hub`-role token whose `hub` field equals the importer's `--origin-hub`
   argument, or every batch gets a `403`.
