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
import type { JsonValue, Message, Notification, PendingAck, Presence } from "./types";

/** Adds the index signature `sql.exec<T>()` requires without repeating it at every call site. */
type Row<T> = T & Record<string, SqlStorageValue>;

const PENDING_ACK_STALE_SECS = 60;

const SCHEMA = `
CREATE TABLE IF NOT EXISTS messages (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  id TEXT NOT NULL UNIQUE,
  client_msg_id TEXT UNIQUE,
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
  origin_hub TEXT,
  origin_seq INTEGER,
  hlc TEXT,
  sensitivity TEXT NOT NULL DEFAULT 'internal'
);
CREATE INDEX IF NOT EXISTS idx_messages_to_ts ON messages(to_agent, timestamp_utc);
CREATE INDEX IF NOT EXISTS idx_messages_from_ts ON messages(from_agent, timestamp_utc);

CREATE TABLE IF NOT EXISTS presence (
  agent TEXT PRIMARY KEY,
  status TEXT NOT NULL,
  protocol_version TEXT NOT NULL,
  timestamp_utc TEXT NOT NULL,
  session_id TEXT NOT NULL,
  capabilities TEXT NOT NULL,
  metadata TEXT,
  ttl_seconds INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL,
  network_context TEXT
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
  network_context TEXT
);
CREATE INDEX IF NOT EXISTS idx_presence_history_agent ON presence_history(agent, timestamp_utc);

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
  origin_hub: string | null;
  origin_seq: number | null;
  hlc: string | null;
  sensitivity: string;
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
    tags: JSON.parse(row.tags) as string[],
    priority: row.priority,
    request_ack: row.request_ack !== 0,
    metadata: row.metadata === null ? null : JSON.parse(row.metadata),
  };
  if (row.thread_id !== null) msg.thread_id = row.thread_id;
  if (row.reply_to !== null) msg.reply_to = row.reply_to;
  if (row.stream_id !== null) msg.stream_id = row.stream_id;
  if (row.client_msg_id !== null) msg.client_msg_id = row.client_msg_id;
  if (row.origin_host !== null) msg.origin_host = row.origin_host;
  if (row.origin_hub !== null) msg.origin_hub = row.origin_hub;
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
  rejected: Array<{ id?: string; client_msg_id?: string; reason: string }>;
  cursor: number;
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
   * `duplicates` split) use the `inserted` flag. */
  insertMessage(input: InsertMessageInput): { message: Message; inserted: boolean } {
    this.ensureSchema();
    const existing = input.client_msg_id
      ? this.sql
          .exec<MessageRow>(
            "SELECT * FROM messages WHERE id = ? OR client_msg_id = ? LIMIT 1",
            input.id,
            input.client_msg_id,
          )
          .toArray()[0]
      : this.sql.exec<MessageRow>("SELECT * FROM messages WHERE id = ? LIMIT 1", input.id).toArray()[0];
    if (existing) return { message: rowToMessage(existing), inserted: false };

    const streamId = input.stream_id ?? null;
    this.sql.exec(
      `INSERT INTO messages
        (id, client_msg_id, timestamp_utc, protocol_version, from_agent, to_agent, topic, body,
         thread_id, tags, priority, request_ack, reply_to, metadata, stream_id,
         origin_host, origin_hub, origin_seq, hlc, sensitivity)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
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
      input.origin_hub ?? null,
      input.origin_seq ?? null,
      input.hlc ?? null,
      input.sensitivity ?? "internal",
    );

    const row = this.sql
      .exec<MessageRow>("SELECT * FROM messages WHERE id = ? LIMIT 1", input.id)
      .toArray()[0];
    if (!row) {
      throw new Error(`inserted message ${input.id} not found after INSERT`);
    }
    if (!row.stream_id) {
      this.sql.exec("UPDATE messages SET stream_id = ? WHERE seq = ?", `${row.seq}-0`, row.seq);
      row.stream_id = `${row.seq}-0`;
    }
    if (input.request_ack) {
      this.sql.exec(
        "INSERT OR REPLACE INTO pending_acks (message_id, recipient, sent_at) VALUES (?, ?, ?)",
        input.id,
        input.to,
        input.timestamp_utc,
      );
    }
    return { message: rowToMessage(row), inserted: true };
  }

  clearPendingAck(messageId: string): void {
    this.ensureSchema();
    this.sql.exec("DELETE FROM pending_acks WHERE message_id = ?", messageId);
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
    for (;;) {
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

      for (const row of rows) {
        if (requiredTags.length > 0) {
          const tags = JSON.parse(row.tags) as string[];
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

  setPresence(input: Presence & { origin_hub?: string }): Presence {
    this.ensureSchema();
    const expiresAtMs = Date.now() + input.ttl_seconds * 1000;
    this.sql.exec(
      `INSERT INTO presence (agent, status, protocol_version, timestamp_utc, session_id, capabilities, metadata, ttl_seconds, expires_at_ms, network_context)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(agent) DO UPDATE SET
         status = excluded.status, protocol_version = excluded.protocol_version,
         timestamp_utc = excluded.timestamp_utc, session_id = excluded.session_id,
         capabilities = excluded.capabilities, metadata = excluded.metadata,
         ttl_seconds = excluded.ttl_seconds, expires_at_ms = excluded.expires_at_ms,
         network_context = excluded.network_context`,
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
    );
    this.sql.exec(
      `INSERT INTO presence_history (agent, status, protocol_version, timestamp_utc, session_id, capabilities, metadata, ttl_seconds, network_context)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      input.agent,
      input.status,
      input.protocol_version,
      input.timestamp_utc,
      input.session_id,
      JSON.stringify(input.capabilities ?? []),
      input.metadata === undefined ? null : JSON.stringify(input.metadata),
      input.ttl_seconds,
      input.network_context ?? null,
    );
    return input;
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

  /** Idempotent batch ingest for `POST /sync/push`. Rejects
   * `sensitivity: "no-offsite"` items (mirrored again here as defense in
   * depth on top of the same check in the HTTP send path) and items missing
   * required fields. Deliberately does NOT run the PHI heuristic screen here
   * (see `containsObviousPhi` in `src/validation.ts`): that screen has real
   * false positives on ordinary review/status text (e.g. "patient name
   * field" in a code comment), and silently dropping an already-approved
   * on-site message during replication would be worse than the residual PHI
   * risk the `sensitivity` opt-out already covers. */
  syncPush(originHub: string, items: InsertMessageInput[]): SyncPushResult {
    this.ensureSchema();
    const accepted: string[] = [];
    const duplicates: string[] = [];
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
      const { inserted } = this.insertMessage({ ...item, origin_hub: item.origin_hub ?? originHub });
      if (inserted) {
        accepted.push(item.id);
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

    return { accepted, duplicates, rejected, cursor: this.currentCursor() };
  }

  /** `GET /sync/pull?since=&exclude_origin=`, paged. */
  syncPull(sinceSeq: number, excludeOrigin: string | undefined, limit: number): { messages: Message[]; next_cursor: number; has_more: boolean } {
    this.ensureSchema();
    const rows = excludeOrigin
      ? this.sql
          .exec<MessageRow>(
            "SELECT * FROM messages WHERE seq > ? AND (origin_hub IS NULL OR origin_hub != ?) ORDER BY seq ASC LIMIT ?",
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

  // -- Health / diagnostics (never exposed on the open /health route) -----

  stats(): { message_count: number; presence_count: number } {
    this.ensureSchema();
    const m = this.sql.exec<Row<{ c: number }>>("SELECT COUNT(*) as c FROM messages").toArray()[0]?.c ?? 0;
    const p = this.sql.exec<Row<{ c: number }>>("SELECT COUNT(*) as c FROM presence").toArray()[0]?.c ?? 0;
    return { message_count: m, presence_count: p };
  }

  override async fetch(): Promise<Response> {
    // This DO is only ever invoked via RPC method calls from the Worker
    // (`stub.insertMessage(...)`, etc.) — it has no HTTP surface of its own.
    return new Response("BusLog Durable Object: RPC only", { status: 404 });
  }
}
