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

- `POST /sync/push` — body `{ origin_hub: string, messages: SendBody[] }`
  (see `src/index.ts`'s `SyncPushMessageInput`, a superset of the normal
  `POST /messages` body with `id`, `timestamp_utc`, `protocol_version`,
  `client_msg_id`, `origin_host`, `origin_hub`, `hlc` all accepted so the
  origin hub's own values are preserved rather than re-minted by the cloud).
  Idempotent on `id` **and** `client_msg_id` (either match is a duplicate,
  reported in the response's `duplicates` array, never re-inserted).
  Messages with `sensitivity: "no-offsite"` are always rejected (see the
  `rejected` array in the response) — the origin hub should filter these out
  before pushing, but the cloud tier enforces it again as defense in depth.
  Batch limit: 500 messages per call.
- `GET /sync/pull?since=<cursor>&exclude_origin=<hub>&limit=<n>` — paged by a
  monotonic integer cursor (`next_cursor` in the response; pass it back as
  the next call's `since`). `exclude_origin` omits messages whose
  `origin_hub` equals the caller's own hub name, so a hub pulling right after
  pushing doesn't re-ingest its own writes. `has_more` tells the caller
  whether another page is available.
- Both endpoints require the same bearer-token auth as every other route
  (`AGENT_BUS_TOKENS` / `AGENT_BUS_AUTH_TOKEN` — see README.md), so the sync
  client authenticates exactly like any other agent-bus caller.

## What this PR does NOT implement

This PR is the cloud tier only. The Rust-side sync client described above —
the async task, the outbox, the claims-proxy switch in `agent-bus-http`'s
`/channels/arbitrate/*` handlers, and the `Health` field additions — is a
**separate, later PR** against `agent-bus-http` / `agent-bus-core`, written
against this contract.
