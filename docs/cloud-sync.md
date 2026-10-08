# Hub-side cloud sync (agent-hub#79, steps 0 and 1)

The on-site `agent-bus-http` hub can replicate its messages and presence to the
cloud tier in `cloud/agentbus` and ingest what other origins wrote there. It is
a background task, **off unless configured**, and it never sits on the request
path. The wire contract is [`cloud/agentbus/SYNC-CONTRACT.md`](../cloud/agentbus/SYNC-CONTRACT.md).

## Enabling it

All three are required. Anything less leaves sync off.

| Setting | Env var | `config.json` key | Notes |
|---|---|---|---|
| Cloud base URL | `AGENT_BUS_CLOUD_URL` | `cloud_url` | `https://` only, except a loopback host (`127.0.0.1`, `::1`, `localhost`). No credentials, query or fragment. |
| Token file | `AGENT_BUS_CLOUD_TOKEN_FILE` | `cloud_token_file` | A file holding the cloud **hub-role** token. Mode `0600` on Unix: a file readable by group or other is refused, not warned about. A separate credential from `auth_token` / `hub.env`. |
| Hub identity | `AGENT_BUS_HUB_IDENTITY` | `hub_identity` | This hub's `origin_hub`, and the `exclude_origin` of every pull. Must equal the `hub` field of the cloud token. |

Optional:

| Setting | Env var | `config.json` key | Default |
|---|---|---|---|
| Cursor file | `AGENT_BUS_CLOUD_SYNC_STATE` | `cloud_sync_state_file` | `~/.config/agent-bus/cloud-sync-state.json` |
| Push history on first run | `AGENT_BUS_CLOUD_SYNC_BACKFILL=1` | | off: first run starts at the current stream tail and the current last presence event |

With none of the URL or token file set, no task starts, nothing is logged, and
`/health` is byte-for-byte what it was before sync existed. If one of the three
is set without the others, or the URL or token file is unusable, sync stays off,
an error is logged at startup, and `/health` shows `cloud_configured: true`,
`cloud_reachable: false` and the reason in `cloud_last_error`.

The token is read once at startup, is never logged, and `Debug` output redacts it.

## What it does

**Push.** Every ~2 s the task reads the Redis stream strictly after a cursor
(`XRANGE (id`, Redis 6.2 or later), keeps the messages that may leave this hub,
and posts them to `/sync/push` in batches of at most 500, using the Worker's
`sender`/`recipient` field names. The cursor is written atomically to the state
file after each acknowledged batch. Presence is pushed from the PostgreSQL
presence history by row id (`origin_id`), so it needs a configured database.

A message may leave only if both hold:

- **It is locally originated**: `origin_hub` is absent or equals this hub. A
  message pulled from the cloud is stored with its own `origin_hub` and is
  therefore never sent back (no echo).
- **Its sensitivity is `internal`**: `sensitivity=no-offsite` is never pushed.
  `Message::effective_sensitivity` fails closed. In order: the `sensitivity`
  field, then `metadata.sensitivity`, then the tag `sensitivity:no-offsite`. Any
  `metadata.sensitivity` other than `internal` or `no-offsite` counts as
  `no-offsite`. Until `POST /messages` takes a first-class `sensitivity`
  field, senders opt out with the metadata key or the tag.

**Pull.** The task calls `/sync/pull?since=<cursor>&exclude_origin=<hub>&limit=200`
from a persisted cursor and follows `has_more`. Each message is written to the
local stream with its original `id`, `origin_hub`, `client_msg_id`, `origin_seq`,
`hlc` and `sensitivity`. Ingest is idempotent on `(origin_hub, id)` through a
Redis `SET NX` marker with a 30 day lifetime, and PostgreSQL rows go through
`ON CONFLICT DO NOTHING` plus the new unique index. Per-recipient notifications
are appended so `check_inbox` sees the message; pending-ack tracking, ownership
tracking and pub/sub are skipped, because the event happened elsewhere. Items
that cannot be parsed are skipped and logged, never fatal.

Pulled presence comes from `GET /presence` every ~15 s and goes to Redis only.
A row is dropped if its `origin_hub` is this hub, if it has expired, or if the
local row for the same agent is as new or newer. It is never added to the
PostgreSQL presence history, so it cannot be pushed back.

**Claims are not synced or proxied.** They are not in the message stream and
the task contains no claim code. A cloud claim is never authoritative for an
on-site resource.

## Failure behaviour

- **Local writes never wait on the cloud.** Nothing on the request path touches
  the sync task. Measured on a real `agent-bus-http` with the cloud killed: 20
  `POST /messages` took 0.029 s with the cloud up and 0.025 s with it down.
- **Bounded outbox.** At most 5000 messages are queued. If a chunk would
  overflow it, the queue is dropped, `cloud_dropped_batches_total` is incremented
  and the task re-reads from the last acknowledged stream id. Nothing is lost
  and memory does not grow.
- **Backoff.** After a failure the task waits `clamp(2^n * U(0,1), 1, 60)`
  seconds (full jitter, ceiling 1, 2, 4, ... 60 s) and resets on success. Each
  HTTP request has a 10 s timeout.
- **Rejections are final.** If the Worker rejects an item (validation, PHI
  screen, size), it is logged and the cursor moves on; retrying cannot help.
- **A corrupt state file stops sync** rather than restarting from zero, which
  would re-push the whole history. Delete it deliberately to reset.

## `/health`

Present only when sync is configured. The fields extend the table in
SYNC-CONTRACT.md section 7.

| Field | Meaning |
|---|---|
| `cloud_configured` | always `true` when the block is present |
| `cloud_reachable` | the last push or pull pass succeeded |
| `cloud_queue_depth` | messages loaded for push but not yet acknowledged |
| `cloud_last_push_at_utc`, `cloud_last_push_age_seconds` | last successful push |
| `cloud_last_pull_at_utc`, `cloud_last_pull_age_seconds` | last successful pull |
| `cloud_dropped_batches_total` | outbox overflows since start |
| `cloud_last_error` | last failure, never containing the token |

## Storage changes (step 0)

- `Message` gains optional `client_msg_id`, `origin_hub`, `origin_seq`, `hlc` and
  `sensitivity`, omitted from JSON when unset.
- The inline PostgreSQL DDL (`ensure_postgres_storage`) adds the same five
  nullable columns with `add column if not exists`, and a **partial** unique
  index on `(origin_hub, client_msg_id) where both are not null`. Old rows never
  enter the index and need no backfill. Message inserts use a bare
  `ON CONFLICT DO NOTHING` so a duplicate `(origin_hub, client_msg_id)` is a
  no-op, not an error. Index names are schema-scoped: custom `message_table`
  names that share a schema with another message table share this index name,
  as they already did for the existing indexes.
- Claim and resource-event keys fold the resource name like the Worker's
  `normalizeResourceName`: backslash to `/`, Unicode lowercase, one leading
  `./` removed; empty and over-256-code-unit names are rejected. Claim keys for
  paths containing uppercase letters therefore change, and a live claim made
  before the upgrade is not found under the new key until its lease expires.

## Not done yet

- `POST /messages`, the batch route and the MCP tools do not yet accept
  `client_msg_id`, `sensitivity`, `origin_seq` or `hlc`, and the hub does not
  mint them. The push relies on `(origin_hub, id)` for idempotency, which the
  Worker already enforces.
- Pulled messages whose `id` is not a UUID are stored in Redis but not in
  PostgreSQL (its `id` column is `uuid`).
- The cloud's own `/channels/arbitrate/*` routes still grant claims. Making them
  read-only or answering `409` is a Worker change in a later step.
- MCP clients cannot use the cloud as a candidate hub (`/mcp` answers 501).
