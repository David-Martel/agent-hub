# Sync contract: on-site hub <-> agentbus.dtmventures.com

This document specifies the contract a future Rust-side sync client (to be added
to `agent-bus-http`, on asuspro13) must honor when talking to the cloud tier
built in this directory. It exists so the cloud tier can be reviewed and
deployed **before** the Rust sync client is written, without the two sides
drifting apart on assumptions.

The design constraint the operator stressed for agent-hub#79 is unconditional:

> local copies of the agent-bus aren't adversely affected by the offsite location.

Concretely, that means every rule below is a hard requirement, not a tuning
knob.

## 1. Async, out of the request path

The sync client is a background task. `POST /messages`, `GET /messages`,
`PUT /presence/:agent`, and every other existing on-site route return to their
caller exactly as fast as they do today — **zero added latency**. The sync
client observes writes (e.g. via the same in-process notification hook that
already feeds SSE subscribers, or by tailing the Redis stream from its last
cursor) and pushes them to the cloud **after** the local write has already
been acknowledged to the original caller.

## 2. Disabled unless configured

No behavior change when nothing is set. The client activates only when both
`AGENT_BUS_CLOUD_URL` (e.g. `https://agentbus.dtmventures.com`) and a bearer
token for it are present in `Settings`. Absence of either must be
indistinguishable from today's hub — same latency, same startup log lines,
same `/health` shape (see §7 for the one additive change).

## 3. Bounded queue with backpressure that drops to "catch up from cursor"

The client keeps a bounded in-memory (or Redis-backed, if durability across
restarts matters more than simplicity) outbox of pending pushes. When the
queue is full:

- **Never** block the write path or apply backpressure to callers.
- **Never** grow the queue unboundedly (this is exactly the failure mode that
  produced the 2026-09-27 20,996-message / 7,815-presence-row island on
  dtm-p1gen7 before it was exported — see agent-hub#78's origin issue).
- **Drop the queue's contents** and record the last successfully-pushed
  `origin_seq` (or local stream id) as the resume point. On the next
  successful flush, replay starts from that cursor via a **catch-up scan**
  (`XRANGE` from the cursor forward) rather than replaying the dropped
  in-memory items individually. This trades a brief burst of resend-from-
  cursor traffic for a hard memory ceiling.

## 4. Exponential backoff

On push failure (`/sync/push` non-2xx, timeout, or connection refused): back
off with jitter, doubling up to a cap (suggested: 1s initial, 2x factor,
60s cap, full jitter). Reset to the initial delay after any successful push.
Never busy-loop against an unreachable cloud tier — DNS-blocked or
network-partitioned agents (see agent-hub#78's roaming-laptop scenario) can be
offline for hours.

## 5. Full local service when the cloud is unreachable

An unreachable cloud tier must never degrade the on-site hub. Concretely:

- `send` / `read` / `presence` / `ack` all continue to work at full speed
  against Redis + PostgreSQL exactly as they do today.
- The only cloud-tier-shaped failure mode visible to a normal caller is that
  `bus_health` (see §7) reports `cloud_reachable: false` and a growing
  `cloud_queue_depth` / `cloud_last_push_age_seconds` — informational, never
  an error returned from `send`/`read`.

## 6. Claims proxy with an explicit lab-scoped fallback

Per the operator's decision in agent-hub#79 (§"Decisions for the operator",
item 2): **the cloud `ClaimDO` is the global claims authority.** The on-site
hub's `POST/GET /channels/arbitrate/:resource` (and `/resolve`, `/renew`,
`/release`) become a **proxy** to `https://agentbus.dtmventures.com/channels/arbitrate/:resource`
when the cloud is reachable.

When the cloud is unreachable, the on-site hub falls back to **granting only
lab-scoped resources** — i.e. it may still serve `claim_resource` requests
locally (using today's Redis-backed `channels::claim_resource_with_options`
logic, unchanged), but:

- Every claim granted in fallback mode carries a marker (e.g.
  `scope_kind: "lab-fallback"` or a dedicated `granted_offline: true` field)
  so agents and the eventual reconciliation pass can tell a lab-scoped grant
  from a globally-authoritative one.
- On reconnect, the local hub must reconcile any fallback-mode claims against
  the cloud authority (last-write-wins is NOT safe for exclusive claims —
  reconciliation should re-submit each fallback claim to the cloud and honor
  whatever the cloud's `recompute_claim_statuses` decides, notifying the
  local holder if it loses).
- Roaming agents (the actual agent-hub#79 motivating case: dtm-p1gen7 off the
  lab LAN with no route to asuspro13 at all) have **no lab-scoped fallback**
  available to them — they see the same offline-claims behavior agent-hub#78
  already specifies (`Exclusive claims are never granted offline`).

## 6a. Hub delegation and identity binding (agent-hub#82)

The cloud tier's auth model (`src/auth.ts`) now binds every write-route actor
field to the caller's bearer-token identity, with one deliberate exception
this section documents: a **hub-role token may vouch for any agent**.

This is required by §6 above: the proxy described there authenticates to
the cloud with ONE hub-role token (asuspro13's own), but forwards claim/
renew/release/ack/knock/message requests on behalf of MANY different
real on-site agents (claude, codex, gemini, ...). If hub-role tokens were
bound the same way agent-role tokens are (body value must equal the token's
own `agent`), every proxied request would be forced to claim/renew/release/
send as the HUB's identity, not the real on-site agent — losing exactly the
end-to-end agent identity the operator's hard requirement calls for.

Concretely:
- **Agent-role tokens are bound.** A body/path value that disagrees with the
  token's own `agent`/`host` is a 403; a matching or omitted value resolves
  to the token's own identity.
- **Hub- and operator-role tokens may vouch** for any non-empty agent value.
  `origin_hub` is the one field even a hub-role token cannot override at
  will beyond its own `hub` — see §6b.
- The proxy from §6 (when it lands) MUST set `sender`/`agent`/`from` to the
  REAL on-site agent's name on every proxied call — vouching is not a license
  to relabel or omit the real actor. This is a trust boundary the proxy
  itself owns; the cloud tier can only verify that some hub-role token
  authorized the request, not that the proxy is telling the truth about
  which on-site agent originated it. That trust is inherent to the design
  (the alternative — one cloud token per on-site agent, kept in sync with
  the fleet's actual agent roster — was rejected as needless fleet-topology
  coupling for a first cut).

## 6a-1. The hub->cloud sync client's token is a SEPARATE credential (2026-09-27)

The proxy/sync client described in §6 and §6a — the on-site `asuspro13`
process that will eventually call `POST /sync/push` / `/sync/push-presence`
/ `GET /sync/pull` against this Worker — authenticates to the **cloud**
tier with its **own dedicated hub-role cloud token**, generated
specifically for that purpose and stored **separately from `hub.env`**
(the file that holds the on-site agent-bus hub's own `AGENT_BUS_AUTH_TOKEN`,
used for on-site fleet auth between asuspro13/the Sparks/dtm-p1gen7, most
recently rotated 2026-09-27 — see the main repo's
`~/.config/agent-bus/config.json`).

These are deliberately two different secrets in two different files:
- **On-site hub token** (`hub.env`): authenticates fleet members to the
  on-site Redis/PostgreSQL-backed hub. Rotating it is a fleet-wide,
  LAN/p2p-scoped operation.
- **Cloud sync token** (its own file, e.g. under `~/.config/agent-bus/` but
  NOT `hub.env`): a hub-role entry in THIS Worker's `AGENT_BUS_TOKENS`
  (`{"agent": "asuspro13-sync", "role": "hub", "hub": "asuspro13"}` per the
  worked example in README.md), reachable from the public internet.
  Rotating it is a cloud-only operation that never touches on-site fleet
  auth.

Reusing the on-site hub token as the cloud sync token (or vice versa) would
mean a compromise of either surface — a leaked cloud secret from a public
endpoint, or a leaked on-site secret from a laptop — compromises BOTH tiers
at once, and makes every future rotation a coupled, higher-risk operation
instead of two independent, low-drama ones. See README.md's "Cloud tokens
are their own credential space" for the equivalent guidance from the cloud
side, and the deploy checklist there for how the real values get generated
and stored (0600, outside the repo, never committed).

## 6b. origin_hub is exclusively hub-identity-derived (agent-hub#82)

`origin_hub` on every `/sync/*` route (§6's proxy included, once it exists)
comes from the hub-role token's OWN `hub` field — never a value the caller
supplies. A whole-request `origin_hub` in the body that disagrees with the
token's `hub` is a 403 for the entire call; a PER-ITEM `origin_hub` (in a
`/sync/push` batch) that disagrees is rejected for that item alone, reported
in the response's `rejected` array, and never silently trusted or silently
overridden. See "Wire contract" below for the exact per-route behavior.

## 7. Metrics and health surfaced in local `/health`

Additive, optional fields on the existing `Health` struct
(`agent_bus_core::models::Health`) — all `#[serde(skip_serializing_if =
"Option::is_none")]` so a hub with sync disabled emits byte-identical
`/health` output to today:

| Field | Type | Meaning |
|---|---|---|
| `cloud_configured` | `bool` | `AGENT_BUS_CLOUD_URL` + token are set |
| `cloud_reachable` | `bool` | last push/health probe succeeded |
| `cloud_queue_depth` | `u64` | pending outbox items |
| `cloud_last_push_at_utc` | `String` | ISO-8601, last successful push |
| `cloud_last_push_age_seconds` | `u64` | derived, for alerting thresholds |
| `cloud_dropped_batches_total` | `u64` | counts §3 queue-drop events since start |

## Wire contract (what this Worker actually accepts)

Every route in this section requires a **hub-role** bearer token (`GET
/sync/pull` also accepts **operator**), and `origin_hub` for the whole
request is ALWAYS the token's own `hub` field — never a value the caller
supplies (agent-hub#82 review H5). A body-level `origin_hub` that disagrees
with the token's `hub` is a 403 for the whole request; a PER-ITEM
`origin_hub` that disagrees is rejected for that item only (see the
`rejected` array), not trusted and not a whole-batch failure.

- `POST /sync/push` — body `{ origin_hub?: string, messages: SendBody[] }`
  (see `src/index.ts`'s `SyncPushMessageInput`, a superset of the normal
  `POST /messages` body with `id`, `timestamp_utc`, `protocol_version`,
  `client_msg_id`, `origin_host`, `origin_hub`, `origin_seq`, `hlc` all
  accepted so the origin hub's own values are preserved rather than re-minted
  by the cloud). Runs the SAME per-item validation as `POST /messages` —
  length/NUL/priority/`tags: string[]`/sensitivity/PHI/size — with schema
  auto-fit deliberately SKIPPED (see §6c below). `id`/`timestamp_utc`/
  `protocol_version` are validated for FORMAT only (not re-minted), so a
  historical row keeps its original values.
  Idempotent on `(origin_hub, id)` **and** `(origin_hub, client_msg_id)`
  (agent-hub#82 review M5 — scoped by origin, not globally unique, so a
  cross-origin id collision can never pre-empt another origin's write).
  A same-origin, same-key hit with IDENTICAL content is a `duplicate`; with
  DIFFERENT content it's a `conflict` — both reported in their own response
  arrays, neither ever overwrites the stored row.
  Messages with `sensitivity: "no-offsite"` are always rejected (see the
  `rejected` array in the response) — the origin hub should filter these out
  before pushing, but the cloud tier enforces it again as defense in depth,
  now FAIL CLOSED on any value other than exactly `internal`/`no-offsite`.
  Batch limit: 500 messages per call. Does NOT create a `pending_acks` row
  for `request_ack: true` items (a bulk historical import of thousands of
  old acks must not create thousands of permanently-stale pending acks).
- `GET /sync/pull?since=<cursor>&exclude_origin=<hub>&limit=<n>` — paged by a
  monotonic integer cursor (`next_cursor` in the response; pass it back as
  the next call's `since`). `exclude_origin` omits messages whose
  `origin_hub` equals the caller's own hub name, so a hub pulling right after
  pushing doesn't re-ingest its own writes. `has_more` tells the caller
  whether another page is available. A non-integer `since` is a `400`, never
  a silently-empty page with `next_cursor: null`.
- `POST /sync/push-presence` (agent-hub#82) — body `{ origin_hub?: string,
  origin_host?: string, events: [{origin_id: int, timestamp_utc,
  protocol_version, agent, status, session_id?, capabilities?, metadata?,
  ttl_seconds?}] }` (<=500 events). Dedups on `(origin_hub, origin_id)` —
  `origin_id` is the ORIGIN'S OWN row id (e.g. the on-site Postgres
  `presence_events.id`), not a message id. Response is `{accepted: int,
  duplicates: int, rejected: [...]}` — COUNTS, not id arrays, matching what
  the historical importer already expects (see below). Only advances the
  "current" `GET /presence` row for `(hub, agent)` when the event's
  `timestamp_utc` is >= what's already stored, so an out-of-order historical
  replay never clobbers live status with stale data.
- `GET /sync/stats` (operator role) — returns per-`origin_hub` message and
  presence counts, for verifying replication volume without reading raw rows.
- All of the above use the same `AGENT_BUS_TOKENS` bearer auth as every other
  route (see README.md's "Auth model"), so the sync client authenticates
  exactly like any other agent-bus caller — it just needs a token with role
  `hub` (and, for the operator-only `/sync/stats`, a separate `operator`
  token).

## 6c. Historical-import compatibility (verified against the actual importer)

`~/.local/share/jules-fleet/handoff-2026-09-26/agentbus-import/
import_pg_export.py` was read in full while building this contract. It
already targets exactly the shapes above — `POST /sync/push` with
`{origin_hub, messages}` and array-shaped `accepted`/`duplicates`/`rejected`,
`POST /sync/push-presence` with `{origin_hub, origin_host, events}` and
INTEGER `accepted`/`duplicates` — and never sends `origin_seq`. **No importer
changes are required**, provided its `AGENTBUS_TOKEN` is configured as a
`hub`-role token whose `hub` field equals the importer's `--origin-hub`
argument.

Schema auto-fit is skipped on `/sync/push` specifically so this importer's
historical rows are not mutated on replay: `autoFitSchema` prepends
`FINDING:`/`SEVERITY:` to an unstructured body inferred to need the
`finding` schema from its topic, which is correct behavior for a FRESH
`POST /messages` but would corrupt an already-accepted on-site row being
imported verbatim years later.

## What this PR does NOT implement

This PR is the cloud tier only. The Rust-side sync client described above —
the async task, the outbox, the claims-proxy switch in `agent-bus-http`'s
`/channels/arbitrate/*` handlers, and the `Health` field additions — is a
**separate, later PR** against `agent-bus-http` / `agent-bus-core`, written
against this contract.
