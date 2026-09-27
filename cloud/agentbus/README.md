# agentbus.dtmventures.com — off-site Cloudflare tier (agent-hub#79)

A Cloudflare Worker that implements the subset of the on-site agent-bus HTTP
API listed in agent-hub#79 (send / read / inbox / presence / claims / ack /
health), plus the new `/sync/push` and `/sync/pull` hub<->cloud replication
endpoints, so the existing `agent-bus` CLI/MCP work unmodified against
`server_url=https://agentbus.dtmventures.com`.

D1 and R2 are **not used**: the Cloudflare API token available for this build
denies both (error 10000). Every table lives in Durable Object **SQLite
storage** (`ctx.storage.sql`) instead — see "Design change" below.

## Route table

Derived directly from `crates/agent-bus-http/src/http.rs`'s router
(`start_http_server`) and the `agent-bus-core::ops` / `channels` modules it
delegates to. "Rust source" names the function(s) this route mirrors.

| Method | Path | Auth | Rust source | Request body | Response |
|---|---|---|---|---|---|
| GET | `/health` | open | `http_health_handler` (redacted subset) | — | `Health` (no fleet counts) |
| POST | `/messages` | bearer | `http_send_handler` -> `validated_post_message` | sender, recipient, topic, body, thread_id?, tags?, priority?, request_ack?, reply_to?, metadata?, schema?, +#79: client_msg_id?, origin_host?, origin_hub?, hlc?, sensitivity? | `Message` |
| GET | `/messages` | bearer | `http_read_handler` -> `list_messages_live` | query: agent, from, topic, repo, session, tag*, thread_id, since, limit, broadcast, excerpt | `Message[]` |
| POST | `/messages/batch` | bearer | `http_batch_send_handler` -> `validated_batch_send` | `{messages: SendBody[]}` (<=100) | `{ids, count}` |
| POST | `/messages/:id/ack` | bearer | `http_ack_handler` -> `post_ack` | `{agent, body?}` | `{ack_sent, ack_message_id, acked_message_id, timestamp}` |
| POST | `/read/batch` | bearer | `http_batch_read_handler` -> `bus_list_messages_from_redis` | `{agents[], since?, limit?, broadcast?}` (<=20 agents) | `Message[]` (deduped, sorted) |
| POST | `/ack/batch` | bearer | `http_batch_ack_handler` | `{agent, message_ids[], body?}` (<=100) | `{acked, message_ids}` |
| POST | `/knock` | bearer | `http_knock_handler` | `{sender, recipient, body?, thread_id?, tags?, request_ack?}` | `Message` (topic=knock, priority=urgent) |
| PUT | `/presence/:agent` | bearer | `http_presence_set_handler` -> `set_presence` | `{status?, session_id?, capabilities?, ttl_seconds?, metadata?}` +#79 `network_context?` | `Presence` |
| GET | `/presence` | bearer | `http_presence_list_handler` -> `ops_list_presence` | — | `Presence[]` (live only) |
| GET | `/presence/history` | bearer | `http_presence_history_handler` -> `list_presence_history_postgres` | query: agent, since, limit | `Presence[]` |
| GET | `/notifications/:agent_id` | bearer | `http_notifications_handler` -> `list_notifications[_since_id]` (approximated — see below) | query: since_id, history | `Notification[]` |
| GET | `/pending-acks` | bearer | `http_pending_acks_handler` -> `ops_list_pending_acks` | query: agent | `PendingAck[]` |
| POST | `/channels/arbitrate/:resource` | bearer | `http_claim_handler` -> `claim_resource_with_options` | `{agent, priority_argument?, mode?, namespace?, scope_kind?, scope_path?, repo_scopes?, thread_id?, lease_ttl_seconds?, scope?}` | `OwnershipClaim` |
| GET | `/channels/arbitrate/:resource` | bearer | `http_arbitration_state_handler` -> `get_arbitration_state` | — | `ArbitrationState` |
| PUT | `/channels/arbitrate/:resource/resolve` | bearer | `http_resolve_handler` -> `resolve_claim` | `{winner, reason?, resolved_by?}` | `ArbitrationState` |
| POST | `/channels/arbitrate/:resource/renew` | bearer | `http_renew_claim_handler` -> `renew_claim` | `{agent, lease_ttl_seconds?}` | `OwnershipClaim` |
| POST | `/channels/arbitrate/:resource/release` | bearer | `http_release_claim_handler` -> `release_claim` | `{agent}` | `ArbitrationState` |
| GET | `/resource-events/:resource_id` | bearer | `http_resource_events_handler` (bonus — cheap given ClaimDO already has the data) | query: limit | `ResourceEvent[]` |
| POST | `/sync/push` | bearer | new, agent-hub#79 | `{origin_hub, messages: SyncPushMessageInput[]}` (<=500) | `{accepted[], duplicates[], rejected[], cursor}` |
| GET | `/sync/pull` | bearer | new, agent-hub#79 | query: since, exclude_origin, limit | `{messages[], next_cursor, has_more}` |

### Deferred (out of the agent-hub#79 cloud subset — return `501`, not `404`)

`/events*` (SSE), `/mcp` (JSON-RPC bridge — the CLI/MCP client speaks plain
REST when `server_url` is set, confirmed by reading
`crates/agent-bus-cli/src/server_mode.rs`, so this was never required),
`/admin/*`, `/channels/direct/*`, `/channels/groups*`, `/channels/escalate`,
`/channels/summary`, `/token-count`, `/compact-context`, `/session-summary`,
`/thread-summary`, `/compact-thread`, `/orchestrator-summary`, `/tasks/*`,
`/subscriptions*`, `/inventory`, `/threads*`, `/overdue-acks`, `/dashboard*`,
`/support`. These match issue #79's own scope table (`send/read/inbox/
presence/claims/ack/health`) — nothing here was silently dropped.

### One documented approximation: `/notifications/:agent_id`

The Rust hub fans every send out to a dedicated per-recipient Redis stream
(`agent_bus:notify:<agent>`) with its own `reason`/`requires_ack` bookkeeping.
The cloud tier derives the same `Notification[]` shape directly from the
message log (`to = agent OR to = 'all'`, newest first) instead of
maintaining a second write path — functionally equivalent for `check_inbox`,
but `id`/`notification_stream_id` are the message's own `seq`, not an
independent notification-stream id.

## Schema additions (agent-hub#79, additive & optional)

`Message` gains `client_msg_id` (UUIDv7), `origin_host`, `origin_hub`,
`origin_seq`, `hlc`, `sync_state` (client-side only), `sensitivity`
(`"internal"` default | `"no-offsite"`). `Presence` gains `network_context`.
None of these change any existing field's name, type, or omission rule — see
`src/types.ts` for the full mapping and `crates/agent-bus-core/tests/
cloud_fixtures.rs` for the Rust-side fixtures this Worker is tested against.

`sensitivity: "no-offsite"` messages are rejected by both `POST /messages`
and `POST /sync/push` (never replicated to the cloud tier). A best-effort PHI
pattern screen (`containsObviousPhi` in `src/validation.ts`) also rejects
obvious SSN/MRN/DOB/patient-identifier patterns — defense in depth, not a
substitute for `sensitivity`.

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

8 test files, 67 tests, all passing:

- `test/health-auth.test.ts` — `/health` open + reveals no fleet data; auth
  required on every other route; both `AGENT_BUS_TOKENS` (per-agent) and
  `AGENT_BUS_AUTH_TOKEN` (shared, parity fallback) accepted; deferred routes
  return 501.
- `test/messages.test.ts` — send/read/batch-send/ack/read-batch/ack-batch/
  knock; schema auto-fit (`finding`/`benchmark`); priority validation;
  `sensitivity: "no-offsite"` rejection; PHI-pattern rejection;
  `client_msg_id` round-trip; broadcast/topic/tag filters.
- `test/presence.test.ts` — set/list/history, `network_context`.
- `test/inbox.test.ts` — `/notifications/:agent_id` (incl. `since_id`
  pagination and `requires_ack`), `/pending-acks` (incl. `stale`).
- `test/claims.test.ts` — claim/renew/release/resolve/get; shared vs.
  exclusive vs. shared_namespaced conflict rules; reroute suggestions
  (generic and machine-global-pattern); **exclusive-claim contention across
  two concurrent `Promise.all` requests** (the DO serializes them: one
  response is "granted", the other "contested", and the stored state then
  shows both "contested" once `recompute_claim_statuses` runs over the full
  set); lease expiry (1-second TTL, pruned on next read); resource-events
  lifecycle; auth.
- `test/sync.test.ts` — push/pull; **idempotent re-push** on `id` and on
  `client_msg_id` alone; `sensitivity: "no-offsite"` rejection; pull
  pagination (`next_cursor`/`has_more`); `exclude_origin` filtering.
- `test/contract.test.ts` — the field-for-field contract test against the
  Rust-generated fixtures in `test/fixtures/*.json` (see below).
- `test/smoke.test.ts` — proves the SQLite-backed DO + Workers RPC path
  works at all under Miniflare before anything else is asserted against it.

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
   npx wrangler secret put AGENT_BUS_AUTH_TOKEN   # optional shared-token fallback
   ```
3. **Bindings** — already declared in `wrangler.toml`, nothing to create by
   hand: `durable_objects.bindings` (`BUS_LOG` -> `BusLog`, `CLAIM_DO` ->
   `ClaimDO`) and the `new_sqlite_classes` migration. No KV, no D1, no R2.
4. **First deploy (smoke test)**:
   ```bash
   npx wrangler deploy
   ```
   This publishes to `<worker-name>.<account-subdomain>.workers.dev`. Verify
   `curl https://<that-url>/health` returns 200 before touching DNS.
5. **Custom domain** — uncomment the `[[routes]]` block in `wrangler.toml`
   (`pattern = "agentbus.dtmventures.com"`, `custom_domain = true`) and
   redeploy. Per `DTMVentures/headscale-ops` policy, `agentbus` is a generic
   name — no fleet device name (asuspro13, spark-*, vigil1, dtm-p1gen7, ...)
   may ever appear in public DNS for this zone.
6. **Point a client at it**: `AGENT_BUS_SERVER_URL=https://agentbus.dtmventures.com`
   (or `server_url` in `~/.config/agent-bus/config.json`) plus a matching
   bearer token from step 2. Confirm with `agent-bus health` /
   `agent-bus presence-list` against that URL.
7. **What the Rust side still needs** (separate, later PR — see
   `SYNC-CONTRACT.md`): the on-site hub's async sync client, and the
   `/channels/arbitrate/*` proxy-to-cloud switch with lab-scoped fallback.
   This Worker is fully usable standalone before that PR lands — it just
   won't yet receive traffic from asuspro13.
