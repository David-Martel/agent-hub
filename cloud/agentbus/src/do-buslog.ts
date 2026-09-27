/**
 * `BusLog` Durable Object — the cloud tier's primary store for the message
 * log, presence, inbox/ack cursors and per-origin sync cursors.
 *
 * D1 and R2 are DENIED on the dtmventures.com Cloudflare API token available
 * to this build (error 10000), so this uses the Durable Object SQLite
 * storage backend (`ctx.storage.sql`, `new_sqlite_classes` migration)
 * instead of D1. A single instance is used at current volume (~27k
 * messages); see SYNC-CONTRACT.md for the sharding note if that changes.
 *
 * All SQL runs synchronously inside the DO's single-threaded execution
 * context, so read-modify-write sequences here (e.g. batch idempotent
 * insert) are atomic without additional locking.
 */

import { DurableObject } from "cloudflare:workers";
import type { Env } from "./env";
import { parseTimestampUtcMs } from "./ids";
import type { JsonValue, Message, Notification, PendingAck, Presence } from "./types";

/** Adds the index signature `sql.exec<T>()` requires without repeating it at every call site. */
type Row<T> = T & Record<string, SqlStorageValue>;

const PENDING_ACK_STALE_SECS = 60;

/** Bounded scan budget for `listMessages`' tag-containment filter (review
 * M2): a tag filter that matches nothing (or a poisoned non-array `tags`
 * row) used to walk the entire 7-day window. This caps total rows examined
 * per call; once exhausted, `listMessages` returns whatever it found rather
 * than continuing to page. */
const MAX_TAG_SCAN_ROWS = 10_000;

/** Fixed-window per-identity rate limit default (review M8/M9's sibling gap:
 * no rate limiting existed anywhere). Overridable via `RATE_LIMIT_PER_MINUTE`. */
export const DEFAULT_RATE_LIMIT_PER_MINUTE = 600;

const SCHEMA = `
CREATE TABLE IF NOT EXISTS messages (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  origin_hub TEXT NOT NULL,
  id TEXT NOT NULL,
  client_msg_id TEXT,
  timestamp_utc TEXT NOT NULL,
  protocol_version TEXT NOT NULL,
  from_agent TEXT NOT NULL,
  to_agent TEXT NOT NULL,
  topic TEXT NOT NULL,
  body TEXT NOT NULL,
  thread_id TEXT,
  tags TEXT NOT NULL,
  priority TEXT NOT NULL,
  request_ack INTEGER NOT NULL,
  reply_to TEXT,
  metadata TEXT,
  stream_id TEXT,
  origin_host TEXT,
  origin_seq INTEGER,
  hlc TEXT,
  sensitivity TEXT NOT NULL DEFAULT 'internal',
  UNIQUE(origin_hub, id),
  UNIQUE(origin_hub, client_msg_id)
);
CREATE INDEX IF NOT EXISTS idx_messages_to_ts ON messages(to_agent, timestamp_utc);
CREATE INDEX IF NOT EXISTS idx_messages_from_ts ON messages(from_agent, timestamp_utc);

-- "Current" presence liveness, keyed by (key_origin, agent) rather than
-- agent alone (review M9): two different hubs' (or two different roaming
-- hosts') "claude" must not collide. key_origin is the hub-role token's
-- hub for events relayed via /sync/push-presence, else the connecting
-- agent-role token's host (falls back to 'unknown-host' when neither is
-- known). It is an internal storage discriminator, never serialized;
-- origin_hub (nullable) is the wire-visible field, set only for relayed
-- presence, mirroring how Message.origin_hub already works.
CREATE TABLE IF NOT EXISTS presence (
  key_origin TEXT NOT NULL,
  agent TEXT NOT NULL,
  status TEXT NOT NULL,
  protocol_version TEXT NOT NULL,
  timestamp_utc TEXT NOT NULL,
  session_id TEXT NOT NULL,
  capabilities TEXT NOT NULL,
  metadata TEXT,
  ttl_seconds INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL,
  network_context TEXT,
  origin_hub TEXT,
  PRIMARY KEY (key_origin, agent)
);

CREATE TABLE IF NOT EXISTS presence_history (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  agent TEXT NOT NULL,
  status TEXT NOT NULL,
  protocol_version TEXT NOT NULL,
  timestamp_utc TEXT NOT NULL,
  session_id TEXT NOT NULL,
  capabilities TEXT NOT NULL,
  metadata TEXT,
  ttl_seconds INTEGER NOT NULL,
  network_context TEXT,
  origin_hub TEXT,
  origin_id INTEGER
);
CREATE INDEX IF NOT EXISTS idx_presence_history_agent ON presence_history(agent, timestamp_utc);

-- Dedup gate for POST /sync/push-presence, keyed on (origin_hub, origin_id)
-- per the on-site export's own row id (review item 6).
CREATE TABLE IF NOT EXISTS presence_sync (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  origin_hub TEXT NOT NULL,
  origin_id INTEGER NOT NULL,
  UNIQUE(origin_hub, origin_id)
);

CREATE TABLE IF NOT EXISTS pending_acks (
  message_id TEXT PRIMARY KEY,
  recipient TEXT NOT NULL,
  sent_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS sync_cursors (
  origin_hub TEXT PRIMARY KEY,
  last_origin_seq INTEGER NOT NULL,
  messages_received INTEGER NOT NULL,
  updated_at TEXT NOT NULL
);

-- Fixed-window rate-limit counters (review M8/M9 sibling gap): one row per
-- (identity_key, window_start_ms). identity_key is role:agent:host -- never
-- a token or token hash (a leaked counter table must not leak credentials).
CREATE TABLE IF NOT EXISTS rate_limit_counters (
  identity_key TEXT NOT NULL,
  window_start_ms INTEGER NOT NULL,
  count INTEGER NOT NULL,
  PRIMARY KEY (identity_key, window_start_ms)
);
`;

interface MessageRow {
  [key: string]: SqlStorageValue;
  seq: number;
  id: string;
  client_msg_id: string | null;
  timestamp_utc: string;
  protocol_version: string;
  from_agent: string;
  to_agent: string;
  topic: string;
  body: string;
  thread_id: string | null;
  tags: string;
  priority: string;
  request_ack: number;
  reply_to: string | null;
  metadata: string | null;
  stream_id: string | null;
  origin_host: string | null;
  origin_hub: string;
  origin_seq: number | null;
  hlc: string | null;
  sensitivity: string;
}

/** Defensively parses the `tags` TEXT column: a non-array or malformed value
 * (which should never be written now that every insert path validates
 * `tags` as `string[]` — see `validateTags` in `validation.ts`) degrades to
 * `[]` instead of crashing every future tag-scoped read (review H7/P6, where
 * a single `tags: 5` row 500'd every `GET /messages?tag=...` for the rest of
 * the 7-day window). */
function parseTagsColumn(raw: string): string[] {
  try {
    const parsed: unknown = JSON.parse(raw);
    return Array.isArray(parsed) ? parsed.filter((t): t is string => typeof t === "string") : [];
  } catch {
    return [];
  }
}

function rowToMessage(row: MessageRow): Message {
  const msg: Message = {
    id: row.id,
    timestamp_utc: row.timestamp_utc,
    protocol_version: row.protocol_version,
    from: row.from_agent,
    to: row.to_agent,
    topic: row.topic,
    body: row.body,
    tags: parseTagsColumn(row.tags),
    priority: row.priority,
    request_ack: row.request_ack !== 0,
    metadata: row.metadata === null ? null : JSON.parse(row.metadata),
  };
  if (row.thread_id !== null) msg.thread_id = row.thread_id;
  if (row.reply_to !== null) msg.reply_to = row.reply_to;
  if (row.stream_id !== null) msg.stream_id = row.stream_id;
  if (row.client_msg_id !== null) msg.client_msg_id = row.client_msg_id;
  if (row.origin_host !== null) msg.origin_host = row.origin_host;
  msg.origin_hub = row.origin_hub;
  if (row.origin_seq !== null) msg.origin_seq = row.origin_seq;
  if (row.hlc !== null) msg.hlc = row.hlc;
  if (row.sensitivity && row.sensitivity !== "internal") {
    msg.sensitivity = row.sensitivity as Message["sensitivity"];
  }
  return msg;
}

export interface InsertMessageInput {
  id: string;
  timestamp_utc: string;
  protocol_version: string;
  from: string;
  to: string;
  topic: string;
  body: string;
  thread_id?: string;
  tags: string[];
  priority: string;
  request_ack: boolean;
  reply_to?: string;
  metadata: JsonValue;
  stream_id?: string;
  client_msg_id?: string;
  origin_host?: string;
  /** The composite dedup key is `UNIQUE(origin_hub, id)` / `UNIQUE(origin_hub,
   * client_msg_id)` (agent-hub#82 review M5), so `insertMessage` REQUIRES a
   * non-empty `origin_hub` at insert time (throws otherwise) even though the
   * field is typed optional here — `syncPush` fills it in from the
   * route-verified hub identity for every item right before inserting, so an
   * individual pre-validated sync item doesn't need to carry its own final
   * value. Direct (non-sync) writes always populate this via
   * `bindOriginHubForDirectWrite` in `index.ts` before it ever reaches here. */
  origin_hub?: string;
  origin_seq?: number;
  hlc?: string;
  sensitivity?: string;
}

export interface ReadMessagesQuery {
  agent?: string;
  from_agent?: string;
  since_ms: number;
  limit: number;
  include_broadcast: boolean;
  topic?: string;
  thread_id?: string;
  required_tags?: string[];
  repo?: string;
  session?: string;
}

export interface SyncPushItem {
  message: InsertMessageInput;
}

export interface SyncPushResult {
  accepted: string[];
  duplicates: string[];
  /** Same key present at the same origin but with DIFFERENT content (review
   * M5). Never overwritten — reported separately from `duplicates` so a
   * replaying client/importer can flag it for human review instead of
   * silently trusting whichever copy arrived first. */
  conflicts: string[];
  rejected: Array<{ id?: string; client_msg_id?: string; reason: string }>;
  cursor: number;
}

/** `true` when `existing` (an already-stored row for the same
 * `(origin_hub, id)` or `(origin_hub, client_msg_id)` key) has different
 * content than `input` — i.e. a genuine conflict, not a harmless retry. */
function messageContentDiffers(existing: MessageRow, input: InsertMessageInput): boolean {
  return (
    existing.from_agent !== input.from ||
    existing.to_agent !== input.to ||
    existing.topic !== input.topic ||
    existing.body !== input.body
  );
}

export class BusLog extends DurableObject<Env> {
  private readonly sql: SqlStorage;
  private initialized = false;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
  }

  private ensureSchema(): void {
    if (this.initialized) return;
    this.sql.exec(SCHEMA);
    this.initialized = true;
    this.maybeScheduleRetention(this.env.RETENTION_DAYS);
  }

  // -- Messages --------------------------------------------------------------

  /** Insert a message, idempotent on `id` OR `client_msg_id`.
   *
   * Returns `{ message, inserted: true }` for a genuinely new message (with
   * `stream_id` set to `seq-0` the way the Redis hub sets it to an XADD
   * stream id), or `{ message: <the existing row>, inserted: false }` when
   * either key already exists. Callers that need idempotent replay semantics
   * (`POST /messages`, `POST /messages/batch`, `POST /sync/push` — the whole
   * point of `client_msg_id` per agent-hub#79 is safe outbox retry) return
   * the existing message rather than erroring; callers that need to know
   * whether a write actually happened (`/sync/push`'s `accepted`/
   * `duplicates`/`conflicts` split) use the `inserted`/`conflict` flags.
   *
   * Idempotency is scoped to `(origin_hub, id)` / `(origin_hub,
   * client_msg_id)` (review M5), not `id`/`client_msg_id` alone: a global
   * unique key let a hostile or misbehaving origin PRE-EMPT another origin's
   * legitimate message by pushing the same id first, silently dropping the
   * real write with no overwrite path to recover it. Scoping by origin
   * means a cross-origin collision on the same key is reported as a
   * `conflict` (when content differs) instead of a phantom `duplicate`.
   * Skips writing a `pending_acks` row when `skipPendingAck` is set (used by
   * `/sync/push` and `/sync/push-presence`-adjacent historical import paths
   * — a bulk import of thousands of old `request_ack` rows must not create
   * thousands of permanently-stale pending acks; see SYNC-CONTRACT.md). */
  insertMessage(
    input: InsertMessageInput,
    opts: { skipPendingAck?: boolean } = {},
  ): { message: Message; inserted: boolean; conflict: boolean } {
    this.ensureSchema();
    if (!input.origin_hub) {
      throw new Error("insertMessage: origin_hub is required");
    }
    const existing = input.client_msg_id
      ? this.sql
          .exec<MessageRow>(
            "SELECT * FROM messages WHERE origin_hub = ? AND (id = ? OR client_msg_id = ?) LIMIT 1",
            input.origin_hub,
            input.id,
            input.client_msg_id,
          )
          .toArray()[0]
      : this.sql
          .exec<MessageRow>(
            "SELECT * FROM messages WHERE origin_hub = ? AND id = ? LIMIT 1",
            input.origin_hub,
            input.id,
          )
          .toArray()[0];
    if (existing) {
      return { message: rowToMessage(existing), inserted: false, conflict: messageContentDiffers(existing, input) };
    }

    const streamId = input.stream_id ?? null;
    this.sql.exec(
      `INSERT INTO messages
        (origin_hub, id, client_msg_id, timestamp_utc, protocol_version, from_agent, to_agent, topic, body,
         thread_id, tags, priority, request_ack, reply_to, metadata, stream_id,
         origin_host, origin_seq, hlc, sensitivity)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      input.origin_hub,
      input.id,
      input.client_msg_id ?? null,
      input.timestamp_utc,
      input.protocol_version,
      input.from,
      input.to,
      input.topic,
      input.body,
      input.thread_id ?? null,
      JSON.stringify(input.tags ?? []),
      input.priority,
      input.request_ack ? 1 : 0,
      input.reply_to ?? null,
      input.metadata === undefined ? null : JSON.stringify(input.metadata),
      streamId,
      input.origin_host ?? null,
      input.origin_seq ?? null,
      input.hlc ?? null,
      input.sensitivity ?? "internal",
    );

    const row = this.sql
      .exec<MessageRow>("SELECT * FROM messages WHERE origin_hub = ? AND id = ? LIMIT 1", input.origin_hub, input.id)
      .toArray()[0];
    if (!row) {
      throw new Error(`inserted message ${input.id} not found after INSERT`);
    }
    if (!row.stream_id) {
      this.sql.exec("UPDATE messages SET stream_id = ? WHERE seq = ?", `${row.seq}-0`, row.seq);
      row.stream_id = `${row.seq}-0`;
    }
    if (input.request_ack && !opts.skipPendingAck) {
      this.sql.exec(
        "INSERT OR REPLACE INTO pending_acks (message_id, recipient, sent_at) VALUES (?, ?, ?)",
        input.id,
        input.to,
        input.timestamp_utc,
      );
    }
    return { message: rowToMessage(row), inserted: true, conflict: false };
  }

  clearPendingAck(messageId: string): void {
    this.ensureSchema();
    this.sql.exec("DELETE FROM pending_acks WHERE message_id = ?", messageId);
  }

  /** Looks up the most recently stored message with this id (any origin —
   * `id` alone is not globally unique post-M5, so this takes the newest
   * match) and returns its recipient (`to_agent`), or `undefined` if no such
   * message exists. Used by `/messages/:id/ack` and `/ack/batch` (review
   * N6) to enforce "only the recipient, or a hub/operator token, may ack a
   * message" BEFORE the ack side effects run: an agent-role caller acking a
   * message addressed to someone else used to return 200 and silently clear
   * the real recipient's pending ack. A message that cannot be found (an
   * unknown/expired id) imposes no restriction — there is nothing to
   * protect, and callers have always been able to ack a bogus id. */
  getMessageRecipient(messageId: string): string | undefined {
    this.ensureSchema();
    const row = this.sql
      .exec<Row<{ to_agent: string }>>(
        "SELECT to_agent FROM messages WHERE id = ? ORDER BY seq DESC LIMIT 1",
        messageId,
      )
      .toArray()[0];
    return row?.to_agent;
  }

  listPendingAcks(agent?: string): PendingAck[] {
    this.ensureSchema();
    const rows = agent
      ? this.sql
          .exec<Row<{ message_id: string; recipient: string; sent_at: string }>>(
            "SELECT message_id, recipient, sent_at FROM pending_acks WHERE recipient = ?",
            agent,
          )
          .toArray()
      : this.sql
          .exec<Row<{ message_id: string; recipient: string; sent_at: string }>>(
            "SELECT message_id, recipient, sent_at FROM pending_acks",
          )
          .toArray();
    const now = Date.now();
    return rows.map((r) => {
      const sentMs = Date.parse(r.sent_at.replace(/(\.\d{3})\d{3}Z$/, "$1Z"));
      const staleSecs = Number.isNaN(sentMs) ? Infinity : (now - sentMs) / 1000;
      return {
        message_id: r.message_id,
        recipient: r.recipient,
        sent_at: r.sent_at,
        stale: staleSecs > PENDING_ACK_STALE_SECS,
      };
    });
  }

  /** Mirrors `message_matches_filters` + `MessageFilters` tag scoping. */
  listMessages(query: ReadMessagesQuery): Message[] {
    this.ensureSchema();
    const cutoffIso = new Date(query.since_ms).toISOString();

    // Every filter EXCEPT tag containment is pushed into the SQL WHERE
    // clause (SQLite's JSON1-free `tags` TEXT column can't index array
    // containment, so that one filter still runs client-side). Rather than a
    // single fixed-size overfetch window — which can silently under-return
    // once a busy period has more matching-on-other-fields-but-not-tags rows
    // than the window holds — this pages backward by `seq` until either
    // `limit` tag-matches are found or the table is exhausted, mirroring
    // `bus_list_messages_from_redis_with_filters`'s own paging loop.
    const includeBroadcast = query.include_broadcast ? 1 : 0;
    const requiredTags = [...(query.required_tags ?? [])];
    if (query.repo) requiredTags.push(`repo:${query.repo}`);
    if (query.session) requiredTags.push(`session:${query.session}`);

    const pageSize = Math.max(query.limit * 5, 200);
    const out: Message[] = [];
    let beforeSeq = Number.MAX_SAFE_INTEGER;
    let scanned = 0;
    for (;;) {
      // Bounded scan budget (review M2): a tag filter matching nothing (or a
      // request tuned to force it) used to page backward through the ENTIRE
      // 7-day window, single-threaded, blocking every other caller of this
      // DO. Once the budget is exhausted, return whatever was found rather
      // than continuing — a partial result under load beats starving the
      // whole tier.
      if (scanned >= MAX_TAG_SCAN_ROWS) break;
      const rows = this.sql
        .exec<MessageRow>(
          `SELECT * FROM messages
           WHERE timestamp_utc >= ?
             AND seq < ?
             AND (? IS NULL OR from_agent = ?)
             AND (? IS NULL OR to_agent = ? OR (? = 1 AND to_agent = 'all'))
             AND (? IS NULL OR topic = ?)
             AND (? IS NULL OR thread_id = ?)
           ORDER BY seq DESC
           LIMIT ?`,
          cutoffIso,
          beforeSeq,
          query.from_agent ?? null,
          query.from_agent ?? null,
          query.agent ?? null,
          query.agent ?? null,
          includeBroadcast,
          query.topic ?? null,
          query.topic ?? null,
          query.thread_id ?? null,
          query.thread_id ?? null,
          pageSize,
        )
        .toArray();
      if (rows.length === 0) break;
      scanned += rows.length;

      for (const row of rows) {
        if (requiredTags.length > 0) {
          const tags = parseTagsColumn(row.tags);
          if (!requiredTags.every((t) => tags.includes(t))) continue;
        }
        out.push(rowToMessage(row));
        if (out.length >= query.limit) break;
      }
      beforeSeq = rows[rows.length - 1]!.seq;
      if (out.length >= query.limit || rows.length < pageSize) break;
    }
    // Pages were fetched newest-first; return chronological order (oldest
    // first), matching `bus_list_messages_from_redis_with_filters`.
    out.reverse();
    return out;
  }

  /** Inbox approximation: messages addressed to `agent`, newest first. The
   * Rust hub fans every send out to a dedicated per-recipient notification
   * stream (`agent_bus:notify:<agent>`) with its own reason/ack metadata;
   * the cloud tier derives the same information from the message log
   * instead of maintaining a second write path. Documented as a deliberate
   * simplification in the PR description. */
  listNotifications(agent: string, sinceId?: string, limit = 20): Notification[] {
    this.ensureSchema();
    let sinceSeq = 0;
    if (sinceId) {
      const parsed = Number.parseInt(sinceId, 10);
      if (!Number.isNaN(parsed)) sinceSeq = parsed;
    }
    const rows = this.sql
      .exec<MessageRow>(
        `SELECT * FROM messages
         WHERE (to_agent = ? OR to_agent = 'all') AND seq > ?
         ORDER BY seq DESC LIMIT ?`,
        agent,
        sinceSeq,
        Math.max(limit, 1),
      )
      .toArray();
    return rows.map((row) => ({
      id: String(row.seq),
      agent,
      created_at: row.timestamp_utc,
      reason: row.request_ack ? "ack_requested" : "message",
      requires_ack: row.request_ack !== 0,
      message: rowToMessage(row),
      notification_stream_id: `${row.seq}-0`,
    }));
  }

  // -- Presence ----------------------------------------------------------

  /** `keyOrigin` discriminates the "current" presence row (review M9): the
   * hub-role token's `hub` for presence relayed via `/sync/push-presence`,
   * else the connecting agent-role token's `host`. Two different hubs' (or
   * hosts') "claude" now occupy different rows instead of overwriting each
   * other. `originHub`, when set, is also surfaced on the wire (mirrors
   * `Message.origin_hub`). */
  setPresence(input: Presence, keyOrigin: string, originHub?: string): Presence {
    this.ensureSchema();
    const expiresAtMs = Date.now() + input.ttl_seconds * 1000;
    this.sql.exec(
      `INSERT INTO presence (key_origin, agent, status, protocol_version, timestamp_utc, session_id, capabilities, metadata, ttl_seconds, expires_at_ms, network_context, origin_hub)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(key_origin, agent) DO UPDATE SET
         status = excluded.status, protocol_version = excluded.protocol_version,
         timestamp_utc = excluded.timestamp_utc, session_id = excluded.session_id,
         capabilities = excluded.capabilities, metadata = excluded.metadata,
         ttl_seconds = excluded.ttl_seconds, expires_at_ms = excluded.expires_at_ms,
         network_context = excluded.network_context, origin_hub = excluded.origin_hub`,
      keyOrigin,
      input.agent,
      input.status,
      input.protocol_version,
      input.timestamp_utc,
      input.session_id,
      JSON.stringify(input.capabilities ?? []),
      input.metadata === undefined ? null : JSON.stringify(input.metadata),
      input.ttl_seconds,
      expiresAtMs,
      input.network_context ?? null,
      originHub ?? null,
    );
    this.sql.exec(
      `INSERT INTO presence_history (agent, status, protocol_version, timestamp_utc, session_id, capabilities, metadata, ttl_seconds, network_context, origin_hub, origin_id)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      input.agent,
      input.status,
      input.protocol_version,
      input.timestamp_utc,
      input.session_id,
      JSON.stringify(input.capabilities ?? []),
      input.metadata === undefined ? null : JSON.stringify(input.metadata),
      input.ttl_seconds,
      input.network_context ?? null,
      originHub ?? null,
      null,
    );
    const result: Presence = { ...input };
    if (originHub) result.origin_hub = originHub;
    return result;
  }

  listPresence(): Presence[] {
    this.ensureSchema();
    const now = Date.now();
    const rows = this.sql
      .exec<Row<{
        agent: string;
        status: string;
        protocol_version: string;
        timestamp_utc: string;
        session_id: string;
        capabilities: string;
        metadata: string | null;
        ttl_seconds: number;
        expires_at_ms: number;
        network_context: string | null;
        origin_hub: string | null;
      }>>("SELECT * FROM presence WHERE expires_at_ms > ?", now)
      .toArray();
    return rows.map((r) => {
      const presence: Presence = {
        agent: r.agent,
        status: r.status,
        protocol_version: r.protocol_version,
        timestamp_utc: r.timestamp_utc,
        session_id: r.session_id,
        capabilities: JSON.parse(r.capabilities) as string[],
        metadata: r.metadata === null ? null : JSON.parse(r.metadata),
        ttl_seconds: r.ttl_seconds,
      };
      if (r.network_context) presence.network_context = r.network_context as Presence["network_context"];
      if (r.origin_hub) presence.origin_hub = r.origin_hub;
      return presence;
    });
  }

  listPresenceHistory(agent: string | undefined, sinceMs: number, limit: number): Presence[] {
    this.ensureSchema();
    const cutoffIso = new Date(sinceMs).toISOString();
    const rows = agent
      ? this.sql
          .exec<Row<{
            agent: string;
            status: string;
            protocol_version: string;
            timestamp_utc: string;
            session_id: string;
            capabilities: string;
            metadata: string | null;
            ttl_seconds: number;
            network_context: string | null;
          }>>(
            "SELECT agent, status, protocol_version, timestamp_utc, session_id, capabilities, metadata, ttl_seconds, network_context FROM presence_history WHERE agent = ? AND timestamp_utc >= ? ORDER BY seq DESC LIMIT ?",
            agent,
            cutoffIso,
            limit,
          )
          .toArray()
      : this.sql
          .exec<Row<{
            agent: string;
            status: string;
            protocol_version: string;
            timestamp_utc: string;
            session_id: string;
            capabilities: string;
            metadata: string | null;
            ttl_seconds: number;
            network_context: string | null;
          }>>(
            "SELECT agent, status, protocol_version, timestamp_utc, session_id, capabilities, metadata, ttl_seconds, network_context FROM presence_history WHERE timestamp_utc >= ? ORDER BY seq DESC LIMIT ?",
            cutoffIso,
            limit,
          )
          .toArray();
    return rows.map((r) => {
      const presence: Presence = {
        agent: r.agent,
        status: r.status,
        protocol_version: r.protocol_version,
        timestamp_utc: r.timestamp_utc,
        session_id: r.session_id,
        capabilities: JSON.parse(r.capabilities) as string[],
        metadata: r.metadata === null ? null : JSON.parse(r.metadata),
        ttl_seconds: r.ttl_seconds,
      };
      if (r.network_context) presence.network_context = r.network_context as Presence["network_context"];
      return presence;
    });
  }

  // -- Sync (agent-hub#79 hub<->cloud replication) ------------------------

  /** The global monotonic cursor for `/sync/pull?since=`. */
  currentCursor(): number {
    this.ensureSchema();
    const row = this.sql.exec<Row<{ m: number | null }>>("SELECT MAX(seq) as m FROM messages").toArray()[0];
    return row?.m ?? 0;
  }

  /** Idempotent batch ingest for `POST /sync/push`. Per-item shared-validator
   * checks (length/priority/tags/sensitivity/PHI/size — review H3/H4/H7) run
   * in `index.ts` BEFORE items reach this method; what's left here is
   * defense-in-depth (`sensitivity`/required-field re-checks in case a
   * caller bypasses the route layer some other way) plus the actual
   * idempotent-insert and per-origin bookkeeping. A per-item `origin_hub`
   * that disagrees with the route-level `originHub` (the hub-role token's
   * OWN hub — review H5) is rejected rather than trusted, so one origin
   * cannot suppress or relabel another's writes. `skipPendingAck: true` on
   * every sync insert (SYNC-CONTRACT.md): a bulk historical import of
   * thousands of old `request_ack` rows must not create thousands of
   * permanently-stale pending acks. Deliberately does NOT run the PHI
   * heuristic screen a second time here — it already ran in `index.ts`. */
  syncPush(originHub: string, items: InsertMessageInput[]): SyncPushResult {
    this.ensureSchema();
    const accepted: string[] = [];
    const duplicates: string[] = [];
    const conflicts: string[] = [];
    const rejected: Array<{ id?: string; client_msg_id?: string; reason: string }> = [];
    let maxOriginSeq = 0;

    for (const item of items) {
      if (item.sensitivity === "no-offsite") {
        rejected.push({ id: item.id, client_msg_id: item.client_msg_id, reason: "sensitivity=no-offsite messages are never replicated to the cloud tier" });
        continue;
      }
      if (!item.from?.trim() || !item.to?.trim() || !item.topic?.trim() || !item.body?.trim()) {
        rejected.push({
          id: item.id,
          client_msg_id: item.client_msg_id,
          reason: "from, to, topic and body must all be non-empty",
        });
        continue;
      }
      if (item.origin_hub && item.origin_hub !== originHub) {
        rejected.push({
          id: item.id,
          client_msg_id: item.client_msg_id,
          reason: `item origin_hub '${item.origin_hub}' does not match the request's origin_hub '${originHub}'`,
        });
        continue;
      }
      const { inserted, conflict } = this.insertMessage(
        { ...item, origin_hub: originHub },
        { skipPendingAck: true },
      );
      if (inserted) {
        accepted.push(item.id);
      } else if (conflict) {
        conflicts.push(item.id);
      } else {
        duplicates.push(item.id);
      }
      if (typeof item.origin_seq === "number") maxOriginSeq = Math.max(maxOriginSeq, item.origin_seq);
    }

    const nowIso = new Date().toISOString();
    const existingCount =
      this.sql
        .exec<Row<{ messages_received: number }>>(
          "SELECT messages_received FROM sync_cursors WHERE origin_hub = ?",
          originHub,
        )
        .toArray()[0]?.messages_received ?? 0;
    this.sql.exec(
      `INSERT INTO sync_cursors (origin_hub, last_origin_seq, messages_received, updated_at)
       VALUES (?, ?, ?, ?)
       ON CONFLICT(origin_hub) DO UPDATE SET
         last_origin_seq = MAX(last_origin_seq, excluded.last_origin_seq),
         messages_received = excluded.messages_received,
         updated_at = excluded.updated_at`,
      originHub,
      maxOriginSeq,
      existingCount + accepted.length,
      nowIso,
    );

    return { accepted, duplicates, conflicts, rejected, cursor: this.currentCursor() };
  }

  /** `GET /sync/pull?since=&exclude_origin=`, paged. */
  syncPull(sinceSeq: number, excludeOrigin: string | undefined, limit: number): { messages: Message[]; next_cursor: number; has_more: boolean } {
    this.ensureSchema();
    const rows = excludeOrigin
      ? this.sql
          .exec<MessageRow>(
            "SELECT * FROM messages WHERE seq > ? AND origin_hub != ? ORDER BY seq ASC LIMIT ?",
            sinceSeq,
            excludeOrigin,
            limit + 1,
          )
          .toArray()
      : this.sql
          .exec<MessageRow>("SELECT * FROM messages WHERE seq > ? ORDER BY seq ASC LIMIT ?", sinceSeq, limit + 1)
          .toArray();
    const hasMore = rows.length > limit;
    const page = hasMore ? rows.slice(0, limit) : rows;
    const lastRow = page.length > 0 ? page[page.length - 1] : undefined;
    const nextCursor = lastRow ? lastRow.seq : sinceSeq;
    return { messages: page.map(rowToMessage), next_cursor: nextCursor, has_more: hasMore };
  }

  /** `POST /sync/push-presence` (agent-hub#82 task item 6): idempotent batch
   * ingest of historical presence events, deduped on `(origin_hub,
   * origin_id)` — the on-site export's own row id, NOT a message id.
   * Writes to `presence_history` (with provenance) unconditionally for new
   * events, and updates the "current" `presence` row for `(originHub,
   * agent)` only when the event is newer than what's already there — an
   * out-of-order historical replay must never clobber a more recent live
   * status. */
  syncPushPresence(
    originHub: string,
    events: Array<{
      origin_id: number;
      timestamp_utc: string;
      protocol_version: string;
      agent: string;
      status: string;
      session_id?: string | null;
      capabilities?: string[];
      metadata?: JsonValue;
      ttl_seconds?: number | null;
    }>,
  ): { accepted: number; duplicates: number; rejected: Array<{ origin_id: number; reason: string }> } {
    this.ensureSchema();
    let accepted = 0;
    let duplicates = 0;
    const rejected: Array<{ origin_id: number; reason: string }> = [];

    for (const ev of events) {
      if (!ev.agent?.trim() || !ev.status?.trim() || !ev.timestamp_utc?.trim()) {
        rejected.push({ origin_id: ev.origin_id, reason: "agent, status and timestamp_utc must all be non-empty" });
        continue;
      }
      const existing = this.sql
        .exec<Row<{ c: number }>>(
          "SELECT count(*) as c FROM presence_sync WHERE origin_hub = ? AND origin_id = ?",
          originHub,
          ev.origin_id,
        )
        .toArray()[0];
      if ((existing?.c ?? 0) > 0) {
        duplicates++;
        continue;
      }
      this.sql.exec(
        "INSERT INTO presence_sync (origin_hub, origin_id) VALUES (?, ?)",
        originHub,
        ev.origin_id,
      );
      const ttlSeconds = ev.ttl_seconds ?? 180;
      this.sql.exec(
        `INSERT INTO presence_history (agent, status, protocol_version, timestamp_utc, session_id, capabilities, metadata, ttl_seconds, network_context, origin_hub, origin_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)`,
        ev.agent,
        ev.status,
        ev.protocol_version,
        ev.timestamp_utc,
        ev.session_id ?? "",
        JSON.stringify(ev.capabilities ?? []),
        ev.metadata === undefined ? null : JSON.stringify(ev.metadata),
        ttlSeconds,
        originHub,
        ev.origin_id,
      );

      const currentRow = this.sql
        .exec<Row<{ timestamp_utc: string }>>(
          "SELECT timestamp_utc FROM presence WHERE key_origin = ? AND agent = ?",
          originHub,
          ev.agent,
        )
        .toArray()[0];
      // Lexicographic comparison is valid here because every timestamp is
      // the same fixed-width `formatTimestampUtc`-shaped ISO-8601 string.
      // Only advance the "current" row when this historical event is
      // actually newer — an out-of-order replay must never clobber live
      // status with stale data.
      if (!currentRow || ev.timestamp_utc >= currentRow.timestamp_utc) {
        const expiresAtMs = (parseTimestampUtcMs(ev.timestamp_utc) ?? Date.now()) + ttlSeconds * 1000;
        this.sql.exec(
          `INSERT INTO presence (key_origin, agent, status, protocol_version, timestamp_utc, session_id, capabilities, metadata, ttl_seconds, expires_at_ms, network_context, origin_hub)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)
           ON CONFLICT(key_origin, agent) DO UPDATE SET
             status = excluded.status, protocol_version = excluded.protocol_version,
             timestamp_utc = excluded.timestamp_utc, session_id = excluded.session_id,
             capabilities = excluded.capabilities, metadata = excluded.metadata,
             ttl_seconds = excluded.ttl_seconds, expires_at_ms = excluded.expires_at_ms,
             origin_hub = excluded.origin_hub`,
          originHub,
          ev.agent,
          ev.status,
          ev.protocol_version,
          ev.timestamp_utc,
          ev.session_id ?? "",
          JSON.stringify(ev.capabilities ?? []),
          ev.metadata === undefined ? null : JSON.stringify(ev.metadata),
          ttlSeconds,
          expiresAtMs,
          originHub,
        );
      }
      accepted++;
    }

    return { accepted, duplicates, rejected };
  }

  // -- Health / diagnostics (never exposed on the open /health route) -----

  stats(): { message_count: number; presence_count: number } {
    this.ensureSchema();
    const m = this.sql.exec<Row<{ c: number }>>("SELECT COUNT(*) as c FROM messages").toArray()[0]?.c ?? 0;
    const p = this.sql.exec<Row<{ c: number }>>("SELECT COUNT(*) as c FROM presence").toArray()[0]?.c ?? 0;
    return { message_count: m, presence_count: p };
  }

  /** `GET /sync/stats` (operator role, agent-hub#82 task item 6): per-origin
   * counts for messages and presence, so an operator can see replication
   * volume by hub without reading raw rows. */
  statsByOrigin(): {
    messages: Array<{ origin_hub: string; count: number }>;
    presence: Array<{ origin_hub: string; count: number }>;
  } {
    this.ensureSchema();
    const messages = this.sql
      .exec<Row<{ origin_hub: string; c: number }>>(
        "SELECT origin_hub, COUNT(*) as c FROM messages GROUP BY origin_hub ORDER BY c DESC",
      )
      .toArray()
      .map((r) => ({ origin_hub: r.origin_hub, count: r.c }));
    const presence = this.sql
      .exec<Row<{ key_origin: string; c: number }>>(
        "SELECT key_origin, COUNT(*) as c FROM presence GROUP BY key_origin ORDER BY c DESC",
      )
      .toArray()
      .map((r) => ({ origin_hub: r.key_origin, count: r.c }));
    return { messages, presence };
  }

  // -- Retention (review M1): a DO alarm, off/long by default -------------

  /** Schedules (or re-schedules) the retention alarm if `RETENTION_DAYS` is
   * a positive integer. Idempotent — safe to call on every write; Durable
   * Object alarms overwrite rather than stack. Off by default (no alarm is
   * ever set when `RETENTION_DAYS` is unset/0/invalid), matching the task's
   * "off or long by default" requirement. */
  maybeScheduleRetention(retentionDays: string | undefined): void {
    const days = Number(retentionDays);
    if (!retentionDays || !Number.isFinite(days) || days <= 0) return;
    // Run roughly daily; the exact cadence doesn't matter as long as it's
    // less than the retention window itself.
    this.ctx.storage.setAlarm(Date.now() + 24 * 60 * 60 * 1000).catch(() => {
      // Best-effort: a failure to schedule the alarm must never fail the
      // write path that triggered this call.
    });
  }

  override async alarm(): Promise<void> {
    this.ensureSchema();
    const retentionDays = Number(this.env.RETENTION_DAYS);
    if (!Number.isFinite(retentionDays) || retentionDays <= 0) return;
    const cutoffIso = new Date(Date.now() - retentionDays * 24 * 60 * 60 * 1000).toISOString();
    this.sql.exec("DELETE FROM messages WHERE timestamp_utc < ?", cutoffIso);
    this.sql.exec("DELETE FROM presence_history WHERE timestamp_utc < ?", cutoffIso);
    // Reschedule for the next window.
    await this.ctx.storage.setAlarm(Date.now() + 24 * 60 * 60 * 1000);
  }

  // -- Rate limiting (review M8/M9 sibling gap) ----------------------------

  /** Fixed-window per-identity counter. `identityKey` MUST be a
   * non-secret discriminator (`role:agent:host`, never a token or token
   * hash — this table is queryable by anyone who can reach the DO's own
   * debug surface in the future, and must never be able to leak a
   * credential). Returns `true` when the caller is within budget (and
   * increments the counter), `false` when the limit for the current
   * 60-second window is already exhausted. */
  checkRateLimit(identityKey: string, limitPerMinute: number): boolean {
    this.ensureSchema();
    const windowStart = Math.floor(Date.now() / 60_000) * 60_000;
    const row = this.sql
      .exec<Row<{ count: number }>>(
        "SELECT count FROM rate_limit_counters WHERE identity_key = ? AND window_start_ms = ?",
        identityKey,
        windowStart,
      )
      .toArray()[0];
    const current = row?.count ?? 0;
    if (current >= limitPerMinute) return false;
    this.sql.exec(
      `INSERT INTO rate_limit_counters (identity_key, window_start_ms, count) VALUES (?, ?, 1)
       ON CONFLICT(identity_key, window_start_ms) DO UPDATE SET count = count + 1`,
      identityKey,
      windowStart,
    );
    // Opportunistic cleanup of old windows (bounded: only ever a handful of
    // rows per identity given the 60s window), so this table doesn't grow
    // unboundedly next to the retention-governed message/presence tables.
    this.sql.exec("DELETE FROM rate_limit_counters WHERE window_start_ms < ?", windowStart - 5 * 60_000);
    return true;
  }

  override async fetch(): Promise<Response> {
    // This DO is only ever invoked via RPC method calls from the Worker
    // (`stub.insertMessage(...)`, etc.) — it has no HTTP surface of its own.
    return new Response("BusLog Durable Object: RPC only", { status: 404 });
  }
}
