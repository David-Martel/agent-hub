/**
 * agentbus.dtmventures.com — the off-site Cloudflare tier of agent-bus
 * (agent-hub#79). REST surface mirroring the subset of the Rust hub's HTTP
 * API listed in the issue: send/read/inbox/presence/claims/ack/health, plus
 * the `/sync/push`, `/sync/pull`, `/sync/push-presence` and `/sync/stats`
 * hub<->cloud replication endpoints (agent-hub#79 + #82).
 *
 * See `cloud/agentbus/README.md` for the deploy checklist and
 * `cloud/agentbus/SYNC-CONTRACT.md` for the contract the Rust-side sync
 * client (a later PR) must honor so the on-site hub is never adversely
 * affected by this tier's existence or reachability.
 *
 * agent-hub#82 security review: this file previously resolved a bearer
 * token to an identity and then never used it — every actor field on every
 * route was taken verbatim from the request body. Every write route below
 * now derives its actor field(s) from the authenticated `Identity` via the
 * `bind*`/`require*` helpers in `auth.ts`. See that file's docblock for the
 * agent/hub/operator role model.
 */

import { Hono } from "hono";
import { handleMcp } from "./mcp";
import { tokenManifest } from "./token-manifest";
import {
  authenticate,
  bindAgent,
  bindOptionalAgent,
  bindOriginHost,
  bindOriginHubForDirectWrite,
  requireHubIdentity,
  requireRole,
  unauthorizedResponse,
  type AuthEnv,
  type Identity,
} from "./auth";
import { DEFAULT_RATE_LIMIT_PER_MINUTE, type BusLog, type InsertMessageInput } from "./do-buslog";
import type { ClaimDO } from "./do-claim";
import { formatTimestampUtc, uuidv7 } from "./ids";
import {
  autoFitSchema,
  assertNoPhi,
  coerceOptionalInt,
  enforceSchemaForTransport,
  MAX_BODY_LEN,
  MAX_LEASE_TTL_SECONDS,
  MAX_ORIGIN_HOST_LEN,
  MAX_ORIGIN_HUB_LEN,
  MAX_RECIPIENT_LEN,
  MAX_SENDER_LEN,
  MAX_SYNC_ID_LEN,
  MAX_THREAD_ID_LEN,
  MIN_LEASE_TTL_SECONDS,
  nonEmpty,
  parseIntParam,
  validateClaimCore,
  validateMessageCore,
  validateMessageSchema,
  validateOptionalString,
  validatePresenceCore,
  validateProtocolVersion,
  validateTags,
  validateTimestampUtc,
  ForbiddenError,
  ValidationError,
  type RawClaimFields,
  type RawMessageFields,
  type RawPresenceFields,
} from "./validation";
import { normalizeResourceName, parseLeaseMode, parseResourceScope } from "./claims-logic";
import type { Health, JsonValue } from "./types";

export { BusLog } from "./do-buslog";
export { ClaimDO } from "./do-claim";

export interface Env extends AuthEnv {
  BUS_LOG: DurableObjectNamespace<BusLog>;
  CLAIM_DO: DurableObjectNamespace<ClaimDO>;
  /** Free-form identity string surfaced in `/health.hub_identity`, and used
   * as the `origin_hub` for every message accepted directly (not via
   * `/sync/push`). Defaults to `"cloud"`. */
  HUB_IDENTITY?: string;
  RETENTION_DAYS?: string;
  RATE_LIMIT_PER_MINUTE?: string;
}

const PROTOCOL_VERSION = "1.0";
const BUILD_VERSION = "agentbus-cloud@0.2.0";
const DEFAULT_SINCE_MINUTES = 60;
const MAX_HISTORY_MINUTES = 10_080; // 7 days, mirrors agent_bus_core::models::MAX_HISTORY_MINUTES
const DEFAULT_LIMIT = 50;

function cloudIdentity(env: Env): string {
  return env.HUB_IDENTITY ?? "cloud";
}

function busLogStub(env: Env) {
  const id = env.BUS_LOG.idFromName("global");
  return env.BUS_LOG.get(id);
}

function claimStub(env: Env, normalizedResource: string) {
  const id = env.CLAIM_DO.idFromName(normalizedResource);
  return env.CLAIM_DO.get(id);
}

/** Response headers applied to every JSON response this Worker returns
 * (agent-hub#82 review M8/LOW): `no-store` so nothing caches a
 * potentially-sensitive bus payload, `nosniff` so a browser never
 * second-guesses the declared `application/json` content type. */
const SECURITY_HEADERS = {
  "cache-control": "no-store",
  "x-content-type-options": "nosniff",
} as const;

/** The on-site hub remains the claims authority. Never dispatch a refused
 * mutation to ClaimDO, including with hub/operator credentials. */
class ClaimsAuthorityError extends Error {
  constructor() {
    super("cloud claims are read-only; use the on-site claims authority");
  }
}

function errorResponse(status: number, message: string): Response {
  return new Response(JSON.stringify({ error: message }), {
    status,
    headers: { "content-type": "application/json", ...SECURITY_HEADERS },
  });
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...SECURITY_HEADERS },
  });
}

/** Runs `fn`, mapping a `ValidationError` to 400, a `ForbiddenError` to 403
 * (agent-hub#82: identity-binding mismatches), and anything else to a
 * GENERIC 500 — review L1: raw SQLite errors ("SQLITE_MISMATCH"), `Date`
 * parsing failures ("Invalid time value") and similar internals used to be
 * echoed straight to the caller. */
async function guarded(fn: () => unknown | Promise<unknown>): Promise<Response> {
  try {
    const result = await fn();
    return json(result);
  } catch (err) {
    if (err instanceof ClaimsAuthorityError) return errorResponse(409, err.message);
    // A `ValidationError`/`ForbiddenError` thrown inside a Durable Object RPC
    // method crosses the DO<->Worker boundary via structured-clone-like
    // serialization, which does not preserve the custom subclass's prototype
    // chain — `.name` (set explicitly in the constructor) survives, but
    // `instanceof` does not. Check both so these are classified the same way
    // whether thrown in-process (route-level validation) or inside a DO.
    const isValidationError =
      err instanceof ValidationError || (err instanceof Error && err.name === "ValidationError");
    if (isValidationError) {
      return errorResponse(400, (err as Error).message);
    }
    const isForbidden = err instanceof ForbiddenError || (err instanceof Error && err.name === "ForbiddenError");
    if (isForbidden) {
      return errorResponse(403, (err as Error).message);
    }
    return errorResponse(500, "internal error");
  }
}

type Bindings = { Bindings: Env; Variables: { identity: Identity } };
const app = new Hono<Bindings>();
// Only requests constructed by our authenticated MCP adapter enter this set.
// Re-authenticate the unchanged bearer on dispatch; count the external RPC once.
const mcpRestRequests = new WeakSet<Request>();

// --- Auth + rate limit middleware: every route except /health --------------

app.use("*", async (c, next) => {
  if (c.req.path === "/health") return next();
  const identity = authenticate(c.req.raw, c.env);
  if (!identity) return unauthorizedResponse();
  c.set("identity", identity);

  if (mcpRestRequests.has(c.req.raw)) return next();

  const limit = Number(c.env.RATE_LIMIT_PER_MINUTE) || DEFAULT_RATE_LIMIT_PER_MINUTE;
  const identityKey = `${identity.role}:${identity.agent}:${identity.host ?? ""}`;
  const withinBudget = await busLogStub(c.env).checkRateLimit(identityKey, limit);
  if (!withinBudget) {
    return new Response(JSON.stringify({ error: "rate limit exceeded" }), {
      status: 429,
      headers: { "content-type": "application/json", "retry-after": "60", ...SECURITY_HEADERS },
    });
  }
  return next();
});

// --- GET /health (unauthenticated, reveals no fleet data) ------------------

app.get("/health", (c) => {
  const health: Health = {
    ok: true,
    protocol_version: PROTOCOL_VERSION,
    build_version: BUILD_VERSION,
    redis_url: "n/a (cloud tier uses Durable Object SQLite storage)",
    database_url: null,
    database_ok: null,
    database_error: null,
    storage_ready: true,
    runtime: "cloudflare-workers",
    codec: "json",
    hub_identity: cloudIdentity(c.env),
  };
  // Deploy-hygiene warning (2026-09-27, on-site hub token rotation
  // follow-up): AGENT_BUS_AUTH_TOKEN is a shared, unbound, lowest-privilege
  // fallback meant ONLY for local dev (see README's "Auth model" and
  // .dev.vars.example) -- it must never be set to a real deployed secret,
  // and it must never be the on-site hub's own AGENT_BUS_AUTH_TOKEN (the
  // token just rotated on asuspro13/both Sparks/dtm-p1gen7): reusing it
  // here would mean a single leaked secret compromises both tiers at once.
  // Surface it at /health so a deploy-time smoke check (or a human) can
  // catch a misconfiguration without grepping Worker logs.
  if (c.env.AGENT_BUS_DEV_ALLOW_SHARED_TOKEN === "1") {
    health.warnings = [
      "AGENT_BUS_DEV_ALLOW_SHARED_TOKEN=1: the shared AGENT_BUS_AUTH_TOKEN auth " +
        "fallback is ENABLED. This is a dev-only escape hatch and must never be set " +
        "in a real deployment; disable it and configure AGENT_BUS_TOKENS instead.",
    ];
  }
  return json(health, 200);
});

// --- POST /messages, GET /messages -----------------------------------------

interface SendBody extends RawMessageFields {
  schema?: string;
  request_ack?: boolean;
  origin_host?: string;
  origin_hub?: string;
}

function buildValidatedMessage(req: SendBody, identity: Identity, env: Env): InsertMessageInput {
  const core = validateMessageCore(req);

  // Schema auto-fit/validation is applied ONLY on this direct-post path, not
  // shared with `/sync/push` (see SYNC-CONTRACT.md and `validateMessageCore`'s
  // docblock in validation.ts): auto-fit MUTATES the body (prepends
  // `FINDING:`/`SEVERITY:`), which is correct for a fresh post but would
  // corrupt an already-accepted historical row on replay.
  const effectiveSchema = enforceSchemaForTransport(req.schema, core.topic);
  const fittedBody = autoFitSchema(core.body, effectiveSchema);
  validateMessageSchema(fittedBody, effectiveSchema);
  // autoFitSchema can prepend template text derived from caller input;
  // re-screen the FINAL body (validateMessageCore already screened the
  // original).
  assertNoPhi(fittedBody);

  const sender = bindAgent(identity, core.sender, "sender");
  // Length-capped BEFORE binding (agent-hub#82 re-review N2): an oversized
  // origin_host/origin_hub in the request body must 400 on its own size,
  // not be silently compared against/overridden by the identity's value.
  const cappedOriginHost = validateOptionalString(req.origin_host, "origin_host", MAX_ORIGIN_HOST_LEN);
  const cappedOriginHub = validateOptionalString(req.origin_hub, "origin_hub", MAX_ORIGIN_HUB_LEN);
  const originHost = bindOriginHost(identity, cappedOriginHost);
  const originHub = bindOriginHubForDirectWrite(identity, cappedOriginHub, cloudIdentity(env));

  const input: InsertMessageInput = {
    id: uuidv7(),
    timestamp_utc: formatTimestampUtc(),
    protocol_version: PROTOCOL_VERSION,
    from: sender,
    to: core.recipient,
    topic: core.topic,
    body: fittedBody,
    tags: core.tags,
    priority: core.priority,
    request_ack: req.request_ack ?? false,
    metadata: core.metadata,
    origin_hub: originHub,
  };
  if (core.thread_id) input.thread_id = core.thread_id;
  if (core.reply_to) input.reply_to = core.reply_to;
  if (core.client_msg_id) input.client_msg_id = core.client_msg_id;
  if (originHost) input.origin_host = originHost;
  if (core.hlc) input.hlc = core.hlc;
  return input;
}

app.post("/messages", async (c) => {
  const body = await c.req.json<SendBody>().catch(() => ({}) as SendBody);
  return guarded(async () => {
    const identity = c.get("identity");
    const input = buildValidatedMessage(body, identity, c.env);
    const stub = busLogStub(c.env);
    // Idempotent on `client_msg_id` (agent-hub#79's whole point for it: a
    // roaming client that timed out and retries a send must get back the
    // ORIGINAL message with 200, not an error) and on `id` (any caller that
    // supplies its own, e.g. a sync replay hitting this route directly).
    const { message } = await stub.insertMessage(input);
    return message;
  });
});

app.get("/messages", async (c) => {
  return guarded(async () => {
    const q = c.req.query();
    const since = parseIntParam(q.since, "since", { min: 0, max: MAX_HISTORY_MINUTES, default: DEFAULT_SINCE_MINUTES });
    const limit = parseIntParam(q.limit, "limit", { min: 1, max: 500, default: DEFAULT_LIMIT });
    const broadcast = q.broadcast === undefined ? true : q.broadcast === "true";
    const tags = new URL(c.req.url).searchParams.getAll("tag");
    const excerpt = q.excerpt !== undefined ? parseIntParam(q.excerpt, "excerpt", { min: 0 }) : undefined;

    const stub = busLogStub(c.env);
    let msgs = await stub.listMessages({
      agent: q.agent,
      from_agent: q.from,
      since_ms: Date.now() - since * 60_000,
      limit,
      include_broadcast: broadcast,
      topic: q.topic,
      thread_id: q.thread_id,
      required_tags: tags,
      repo: q.repo,
      session: q.session,
    });
    if (excerpt !== undefined) {
      msgs = msgs.map((m) => ({ ...m, body: m.body.length > excerpt ? `${m.body.slice(0, excerpt)}…` : m.body }));
    }
    return msgs;
  });
});

// --- POST /messages/batch ---------------------------------------------------

app.post("/messages/batch", async (c) => {
  const body = await c.req.json<{ messages?: SendBody[] }>().catch(() => ({}) as { messages?: SendBody[] });
  return guarded(async () => {
    const identity = c.get("identity");
    const messages = body.messages ?? [];
    if (messages.length === 0) throw new ValidationError("messages array must not be empty");
    if (messages.length > 100) throw new ValidationError("batch size limit is 100 messages");

    // Re-review N8: validate EVERY item BEFORE writing ANY of them. The old
    // code validated-then-inserted one item at a time, so item 0 persisted
    // even when item 1 failed validation (verified: a 400 response left a
    // partially-applied batch behind). Preserve the original error's status
    // (a `ForbiddenError` from an identity-binding mismatch stays a 403,
    // not a 400 -- the other N8 residual the review flagged) while still
    // naming which item failed.
    const inputs: InsertMessageInput[] = messages.map((item, idx) => {
      try {
        return buildValidatedMessage(item, identity, c.env);
      } catch (err) {
        if (err instanceof ForbiddenError) {
          throw new ForbiddenError(`item ${idx}: ${err.message}`);
        }
        const msg = err instanceof Error ? err.message : String(err);
        throw new ValidationError(`item ${idx}: ${msg}`);
      }
    });

    // All inserts happen in ONE synchronous Durable Object RPC
    // (`insertMessageBatch`), which Cloudflare's SQLite storage backend
    // wraps in a single implicit transaction (no `await` occurs between the
    // individual `INSERT`s): if any insert throws, every insert already
    // made earlier in this same call rolls back too. A same-origin
    // duplicate (same id/client_msg_id) is NOT an error -- it's reported as
    // `inserted: false` per item, matching the non-batch route's own
    // idempotent-replay semantics, so it does not roll back its siblings.
    const stub = busLogStub(c.env);
    const results = await stub.insertMessageBatch(inputs);
    const ids = results.map((r) => r.message.id);
    return { ids, count: ids.length };
  });
});

// --- POST /messages/:id/ack --------------------------------------------------

app.post("/messages/:id/ack", async (c) => {
  const messageId = c.req.param("id").trim();
  const body = await c.req.json<{ agent?: string; body?: string }>().catch(() => ({} as { agent?: string; body?: string }));
  return guarded(async () => {
    const identity = c.get("identity");
    // "Ack only for yourself" (agent-hub#82 task): an agent-role token can
    // only ack as itself; hub/operator tokens may vouch (relaying an
    // on-site ack).
    const agent = bindAgent(identity, body.agent, "agent");
    if (!messageId) throw new ValidationError("message id must not be empty");
    const ackBody = body.body ?? "ack";
    assertNoPhi(ackBody);

    const stub = busLogStub(c.env);
    // "Ack only the recipient, or hub/operator" (re-review N6): an
    // agent-role token vouches for nobody but itself (bindAgent above
    // already enforced that), so it may only clear an ack for a message
    // actually addressed to it, or a broadcast. hub/operator tokens
    // vouch for any on-site agent and bypass this check.
    if (identity.role === "agent") {
      const recipient = await stub.getMessageRecipient(messageId);
      if (recipient !== undefined && recipient !== agent && recipient !== "all") {
        throw new ForbiddenError(`ack rejected: '${agent}' is not the recipient of message '${messageId}'`);
      }
    }
    const { message: ackMessage } = await stub.insertMessage({
      id: uuidv7(),
      timestamp_utc: formatTimestampUtc(),
      protocol_version: PROTOCOL_VERSION,
      from: agent,
      to: "all",
      topic: "ack",
      body: ackBody,
      tags: [],
      priority: "normal",
      request_ack: false,
      reply_to: messageId,
      metadata: { ack_for: messageId },
      origin_hub: bindOriginHubForDirectWrite(identity, undefined, cloudIdentity(c.env)),
    });
    await stub.clearPendingAck(messageId);
    return {
      ack_sent: true,
      ack_message_id: ackMessage.id,
      acked_message_id: messageId,
      timestamp: ackMessage.timestamp_utc,
    };
  });
});

// --- POST /read/batch --------------------------------------------------------

app.post("/read/batch", async (c) => {
  const body = await c.req
    .json<{ agents?: string[]; since?: number; limit?: number; broadcast?: boolean }>()
    .catch(() => ({} as { agents?: string[]; since?: number; limit?: number; broadcast?: boolean }));
  return guarded(async () => {
    const agents = body.agents ?? [];
    if (agents.length === 0) throw new ValidationError("agents array must not be empty");
    if (agents.length > 20) throw new ValidationError("batch read limit is 20 agents");
    const since = Math.min(coerceOptionalInt(body.since, "since", { min: 0 }) ?? DEFAULT_SINCE_MINUTES, MAX_HISTORY_MINUTES);
    const limit = Math.min(Math.max(coerceOptionalInt(body.limit, "limit", { min: 1 }) ?? DEFAULT_LIMIT, 1), 500);
    const broadcast = body.broadcast ?? true;

    const stub = busLogStub(c.env);
    const seen = new Set<string>();
    const all = [];
    for (const agent of agents) {
      const msgs = await stub.listMessages({
        agent,
        since_ms: Date.now() - since * 60_000,
        limit,
        include_broadcast: broadcast,
      });
      for (const m of msgs) {
        if (!seen.has(m.id)) {
          seen.add(m.id);
          all.push(m);
        }
      }
    }
    all.sort((a, b) => a.timestamp_utc.localeCompare(b.timestamp_utc));
    return all;
  });
});

// --- POST /ack/batch ----------------------------------------------------------

app.post("/ack/batch", async (c) => {
  const body = await c.req
    .json<{ agent?: string; message_ids?: string[]; body?: string }>()
    .catch(() => ({} as { agent?: string; message_ids?: string[]; body?: string }));
  return guarded(async () => {
    const identity = c.get("identity");
    const agent = bindAgent(identity, body.agent, "agent");
    const ids = body.message_ids ?? [];
    if (ids.length === 0) throw new ValidationError("message_ids must not be empty");
    if (ids.length > 100) throw new ValidationError("batch ack limit is 100 message IDs");
    const ackBody = body.body ?? "ack";
    assertNoPhi(ackBody);

    const stub = busLogStub(c.env);
    const originHub = bindOriginHubForDirectWrite(identity, undefined, cloudIdentity(c.env));
    // Re-review N6, applied to the batch route: validate EVERY id's
    // recipient before acking ANY of them, so one unauthorized id in the
    // middle of a batch cannot leave earlier acks already applied while the
    // request as a whole fails. This does not give full atomicity against
    // other mid-batch failures (that is N8's scope for /messages/batch) but
    // it does close the specific "acked some, then 403'd" gap for this
    // authorization check.
    if (identity.role === "agent") {
      for (const messageId of ids) {
        const recipient = await stub.getMessageRecipient(messageId);
        if (recipient !== undefined && recipient !== agent && recipient !== "all") {
          throw new ForbiddenError(`ack rejected: '${agent}' is not the recipient of message '${messageId}'`);
        }
      }
    }
    const acked: string[] = [];
    for (const messageId of ids) {
      await stub.insertMessage({
        id: uuidv7(),
        timestamp_utc: formatTimestampUtc(),
        protocol_version: PROTOCOL_VERSION,
        from: agent,
        to: "all",
        topic: "ack",
        body: ackBody,
        tags: [],
        priority: "normal",
        request_ack: false,
        reply_to: messageId,
        metadata: { ack_for: messageId },
        origin_hub: originHub,
      });
      await stub.clearPendingAck(messageId);
      acked.push(messageId);
    }
    return { acked: acked.length, message_ids: acked };
  });
});

// --- POST /knock ---------------------------------------------------------------

app.post("/knock", async (c) => {
  const body = await c.req
    .json<{ sender?: string; recipient?: string; body?: string; thread_id?: string; tags?: unknown; request_ack?: boolean }>()
    .catch(() => ({} as { sender?: string; recipient?: string; body?: string; thread_id?: string; tags?: unknown; request_ack?: boolean }));
  return guarded(async () => {
    const identity = c.get("identity");
    const sender = bindAgent(identity, body.sender, "sender");
    const recipient = nonEmpty(body.recipient ?? "", "recipient", MAX_RECIPIENT_LEN);
    const knockBody = nonEmpty(body.body ?? "check the bus", "body", MAX_BODY_LEN);
    const threadId = validateOptionalString(body.thread_id, "thread_id", MAX_THREAD_ID_LEN);
    const tags = validateTags(body.tags);
    assertNoPhi(knockBody, recipient, threadId);
    const requestAck = body.request_ack ?? true;

    const stub = busLogStub(c.env);
    const input: InsertMessageInput = {
      id: uuidv7(),
      timestamp_utc: formatTimestampUtc(),
      protocol_version: PROTOCOL_VERSION,
      from: sender,
      to: recipient,
      topic: "knock",
      body: knockBody,
      tags,
      priority: "urgent",
      request_ack: requestAck,
      metadata: {
        knock: true,
        delivery_hint: "sse",
        expected_response_kind: requestAck ? "ack" : "status",
      },
      origin_hub: bindOriginHubForDirectWrite(identity, undefined, cloudIdentity(c.env)),
    };
    if (threadId) input.thread_id = threadId;
    const { message } = await stub.insertMessage(input);
    return message;
  });
});

// --- PUT /presence/:agent, GET /presence, GET /presence/history ----------------

function presenceOrigin(identity: Identity): { keyOrigin: string; originHub?: string } {
  if (identity.role === "hub" && identity.hub) {
    return { keyOrigin: identity.hub, originHub: identity.hub };
  }
  return { keyOrigin: identity.host ?? "unknown-host" };
}

app.put("/presence/:agent", async (c) => {
  const agentParam = c.req.param("agent").trim();
  const body = await c.req
    .json<RawPresenceFields & { ttl_seconds?: number }>()
    .catch(() => ({}) as RawPresenceFields & { ttl_seconds?: number });
  return guarded(async () => {
    const identity = c.get("identity");
    const agent = bindAgent(identity, agentParam, "agent");
    const core = validatePresenceCore(body);
    const ttl = Math.min(Math.max(coerceOptionalInt(body.ttl_seconds, "ttl_seconds", { min: 1 }) ?? 180, 1), 86_400);
    const stub = busLogStub(c.env);
    const { keyOrigin, originHub } = presenceOrigin(identity);
    return stub.setPresence(
      {
        agent,
        status: core.status,
        protocol_version: PROTOCOL_VERSION,
        timestamp_utc: formatTimestampUtc(),
        session_id: core.session_id,
        capabilities: core.capabilities,
        metadata: core.metadata,
        ttl_seconds: ttl,
        network_context: core.network_context as never,
      },
      keyOrigin,
      originHub,
    );
  });
});

app.get("/presence", async (c) => {
  return guarded(async () => busLogStub(c.env).listPresence());
});

app.get("/presence/history", async (c) => {
  return guarded(async () => {
    const q = c.req.query();
    const since = parseIntParam(q.since, "since", { min: 0, max: MAX_HISTORY_MINUTES, default: DEFAULT_SINCE_MINUTES });
    const limit = parseIntParam(q.limit, "limit", { min: 1, max: 500, default: DEFAULT_LIMIT });
    return busLogStub(c.env).listPresenceHistory(q.agent, Date.now() - since * 60_000, limit);
  });
});

// --- GET /notifications/:agent_id (inbox), GET /pending-acks -------------------

app.get("/notifications/:agent_id", async (c) => {
  const agentId = c.req.param("agent_id");
  return guarded(async () => {
    const q = c.req.query();
    const history = parseIntParam(q.history, "history", { min: 1, max: 500, default: 20 });
    return busLogStub(c.env).listNotifications(agentId, q.since_id, history);
  });
});

app.get("/pending-acks", async (c) => {
  return guarded(async () => busLogStub(c.env).listPendingAcks(c.req.query("agent")));
});

// --- Claims: POST/GET /channels/arbitrate/:resource, resolve/renew/release -----

interface ClaimBody extends RawClaimFields {
  agent?: string;
  mode?: string;
  lease_ttl_seconds?: number;
  scope?: string;
}

app.post("/channels/arbitrate/:resource", async (c) => {
  const rawResource = c.req.param("resource");
  const body = await c.req.json<ClaimBody>().catch(() => ({}) as ClaimBody);
  return guarded(async () => {
    // Validated inside `guarded()` (agent-hub#82 review L1/M6 follow-up): an
    // oversized/empty resource name must 400 through the normal JSON error
    // path, not escape as Hono's default plain-text 500 the way it did when
    // this validation ran before `guarded` was ever entered.
    normalizeResourceName(rawResource);
    const identity = c.get("identity");
    bindAgent(identity, body.agent, "agent");
    const mode = parseLeaseMode(body.mode ?? "exclusive");
    if (body.scope) parseResourceScope(body.scope);
    coerceOptionalInt(body.lease_ttl_seconds, "lease_ttl_seconds", {
      min: MIN_LEASE_TTL_SECONDS,
      max: MAX_LEASE_TTL_SECONDS,
    });
    // Type-checks and caps namespace/scope_kind/scope_path/repo_scopes/
    // priority_argument/thread_id (agent-hub#82 re-review N4: `namespace:{}`
    // used to 500 deep inside the DO, and `repo_scopes:"abc"` was stored and
    // served back as a bare string).
    const claimFields = validateClaimCore(body);
    // This cross-field constraint previously ran inside ClaimDO. Preserve
    // its input rejection without touching the object's retained state.
    if (mode === "shared_namespaced" && !(claimFields.namespace ?? "").trim()) {
      throw new ValidationError("shared_namespaced claims require --namespace");
    }
    throw new ClaimsAuthorityError();
  });
});

app.get("/channels/arbitrate/:resource", async (c) => {
  const rawResource = c.req.param("resource");
  return guarded(async () => {
    const resource = normalizeResourceName(rawResource);
    return claimStub(c.env, resource).getState(resource);
  });
});

app.put("/channels/arbitrate/:resource/resolve", async (c) => {
  const rawResource = c.req.param("resource");
  const body = await c.req.json<{ winner?: string; reason?: string; resolved_by?: string }>().catch(() => ({} as { winner?: string; reason?: string; resolved_by?: string }));
  return guarded(async () => {
    normalizeResourceName(rawResource);
    const identity = c.get("identity");
    // Preserve the original operator-only resolve guard before refusing
    // Cloud mutations. The on-site authority owns arbitration decisions.
    requireRole(identity, "operator");
    const winner = (body.winner ?? "").trim();
    if (!winner) throw new ValidationError("winner must not be empty");
    bindOptionalAgent(identity, body.resolved_by);
    throw new ClaimsAuthorityError();
  });
});

app.post("/channels/arbitrate/:resource/renew", async (c) => {
  const rawResource = c.req.param("resource");
  const body = await c.req.json<{ agent?: string; lease_ttl_seconds?: number }>().catch(() => ({} as { agent?: string; lease_ttl_seconds?: number }));
  return guarded(async () => {
    normalizeResourceName(rawResource);
    const identity = c.get("identity");
    // Owner-only (agent-hub#82 review H2/P2): an agent-role token can only
    // renew its OWN claim; hub/operator tokens may vouch for the on-site
    // agent they're relaying for.
    bindAgent(identity, body.agent, "agent");
    coerceOptionalInt(body.lease_ttl_seconds, "lease_ttl_seconds", {
      min: MIN_LEASE_TTL_SECONDS,
      max: MAX_LEASE_TTL_SECONDS,
    });
    throw new ClaimsAuthorityError();
  });
});

app.post("/channels/arbitrate/:resource/release", async (c) => {
  const rawResource = c.req.param("resource");
  const body = await c.req.json<{ agent?: string }>().catch(() => ({} as { agent?: string }));
  return guarded(async () => {
    normalizeResourceName(rawResource);
    const identity = c.get("identity");
    // Owner-only, same rule as renew.
    bindAgent(identity, body.agent, "agent");
    throw new ClaimsAuthorityError();
  });
});

app.get("/resource-events/:resource_id", async (c) => {
  const rawResource = c.req.param("resource_id");
  return guarded(async () => {
    const resource = normalizeResourceName(rawResource);
    const limit = parseIntParam(c.req.query("limit"), "limit", { min: 1, max: 1000, default: 100 });
    return claimStub(c.env, resource).listEvents(limit);
  });
});

// --- Sync (agent-hub#79/#82): hub<->cloud replication --------------------------

interface SyncPushMessageInput extends RawMessageFields {
  id?: string;
  timestamp_utc?: string;
  protocol_version?: string;
  thread_id?: string;
  request_ack?: boolean;
  reply_to?: string;
  client_msg_id?: string;
  origin_host?: string;
  origin_hub?: string;
  origin_seq?: number;
  hlc?: string;
}

/**
 * Builds and validates one `/sync/push` item using the SAME shared core
 * validator as `/messages` (`validateMessageCore` — length/NUL/priority/
 * tags/sensitivity/PHI/size checks), plus format validation for the fields a
 * historical import is allowed to keep as-is: `id`, `timestamp_utc` and
 * `protocol_version` (agent-hub#82 task item 2). Deliberately skips schema
 * auto-fit/validation — see `buildValidatedMessage`'s docblock and
 * SYNC-CONTRACT.md. Throws `ValidationError` on any violation; the caller
 * catches this per item so one bad row in a batch doesn't fail the whole
 * push.
 */
function buildSyncPushItem(m: SyncPushMessageInput): InsertMessageInput {
  const core = validateMessageCore(m);
  // Unlike direct `/messages`, a synced item has no caller "identity" a
  // missing sender could default to — a hub token vouches for an EXPLICIT
  // historical agent name, never its own (agent-hub#82 re-review N9's
  // resolution: `sender` is optional in the shared core so `/messages` can
  // fill it from the token, but `/sync/push` still requires it outright).
  if (!core.sender) throw new ValidationError("sender must not be empty");

  const id = m.id && m.id.trim() ? m.id.trim() : uuidv7();
  if (id.length > MAX_SYNC_ID_LEN) throw new ValidationError(`id exceeds maximum length of ${MAX_SYNC_ID_LEN}`);
  if (id.includes("\u0000")) throw new ValidationError("id must not contain NUL bytes (\\x00)");

  // Canonicalized to `formatTimestampUtc`'s exact shape (agent-hub#82
  // re-review N10): a stored raw value like "2020-01-01 00:00:00+05:00"
  // (space separator, non-Z offset) parses fine via `Date.parse` but sorts
  // and compares incorrectly against every other row's canonical string —
  // it was invisible to `since=60`-style cutoff queries and would corrupt
  // retention/presence "is this newer" comparisons.
  const timestampUtc = m.timestamp_utc ? validateTimestampUtc(m.timestamp_utc) : formatTimestampUtc();
  const protocolVersion = m.protocol_version ? validateProtocolVersion(m.protocol_version) : PROTOCOL_VERSION;
  const originSeq = coerceOptionalInt(m.origin_seq, "origin_seq", { min: 0 });
  const originHost = validateOptionalString(m.origin_host, "origin_host", MAX_ORIGIN_HOST_LEN);
  // Length-capped but NOT bound/defaulted here (agent-hub#82 re-review N2 +
  // H5) — `BusLog.syncPush` compares this against the route-verified hub
  // identity and rejects a per-item mismatch rather than silently trusting
  // or silently overriding it.
  const originHub = validateOptionalString(m.origin_hub, "origin_hub", MAX_ORIGIN_HUB_LEN);

  const input: InsertMessageInput = {
    id,
    timestamp_utc: timestampUtc,
    protocol_version: protocolVersion,
    from: core.sender,
    to: core.recipient,
    topic: core.topic,
    body: core.body,
    tags: core.tags,
    priority: core.priority,
    request_ack: m.request_ack ?? false,
    metadata: core.metadata,
  };
  if (core.thread_id) input.thread_id = core.thread_id;
  if (core.reply_to) input.reply_to = core.reply_to;
  if (core.client_msg_id) input.client_msg_id = core.client_msg_id;
  if (originHost) input.origin_host = originHost;
  if (originHub) input.origin_hub = originHub;
  if (core.hlc) input.hlc = core.hlc;
  if (originSeq !== undefined) input.origin_seq = originSeq;
  if (core.sensitivity !== "internal") input.sensitivity = core.sensitivity;
  return input;
}

app.post("/sync/push", async (c) => {
  const body = await c.req
    .json<{ origin_hub?: string; messages?: SyncPushMessageInput[] }>()
    .catch(() => ({} as { origin_hub?: string; messages?: SyncPushMessageInput[] }));
  return guarded(async () => {
    const identity = c.get("identity");
    // /sync/* requires a hub-role token; origin_hub for the whole request
    // comes from the TOKEN, never the body (review H1/H5).
    const originHub = requireHubIdentity(identity);
    if (body.origin_hub && body.origin_hub !== originHub) {
      throw new ForbiddenError(`origin_hub '${body.origin_hub}' does not match the bearer token's hub '${originHub}'`);
    }
    const rawMessages = body.messages ?? [];
    if (rawMessages.length > 500) throw new ValidationError("sync push batch limit is 500 messages");

    const items: InsertMessageInput[] = [];
    const preRejected: Array<{ id?: string; client_msg_id?: string; reason: string }> = [];
    for (const m of rawMessages) {
      try {
        items.push(buildSyncPushItem(m));
      } catch (err) {
        const reason = err instanceof Error ? err.message : String(err);
        preRejected.push({ id: m.id, client_msg_id: m.client_msg_id, reason });
      }
    }

    const result = await busLogStub(c.env).syncPush(originHub, items);
    return { ...result, rejected: [...preRejected, ...result.rejected] };
  });
});

app.get("/sync/pull", async (c) => {
  return guarded(async () => {
    const identity = c.get("identity");
    // Restricted to hub or operator tokens (review M3): this was a
    // full-archive export available to ANY authenticated caller.
    requireRole(identity, "hub", "operator");
    const q = c.req.query();
    const since = parseIntParam(q.since, "since", { min: 0, default: 0 });
    const excludeOrigin = q.exclude_origin || undefined;
    const limit = parseIntParam(q.limit, "limit", { min: 1, max: 1000, default: 200 });
    return busLogStub(c.env).syncPull(since, excludeOrigin, limit);
  });
});

interface SyncPushPresenceEvent {
  origin_id?: number;
  timestamp_utc?: string;
  protocol_version?: string;
  agent?: string;
  status?: string;
  session_id?: string;
  capabilities?: string[];
  metadata?: JsonValue;
  ttl_seconds?: number;
}

app.post("/sync/push-presence", async (c) => {
  const body = await c.req
    .json<{ origin_hub?: string; origin_host?: string; events?: SyncPushPresenceEvent[] }>()
    .catch(() => ({} as { origin_hub?: string; origin_host?: string; events?: SyncPushPresenceEvent[] }));
  return guarded(async () => {
    const identity = c.get("identity");
    const originHub = requireHubIdentity(identity);
    if (body.origin_hub && body.origin_hub !== originHub) {
      throw new ForbiddenError(`origin_hub '${body.origin_hub}' does not match the bearer token's hub '${originHub}'`);
    }
    const rawEvents = body.events ?? [];
    if (rawEvents.length > 500) throw new ValidationError("sync push-presence batch limit is 500 events");

    const events: Array<{
      origin_id: number;
      timestamp_utc: string;
      protocol_version: string;
      agent: string;
      status: string;
      session_id?: string | null;
      capabilities?: string[];
      metadata?: JsonValue;
      ttl_seconds?: number | null;
    }> = [];
    const preRejected: Array<{ origin_id: number; reason: string }> = [];
    for (const ev of rawEvents) {
      try {
        if (ev.origin_id === undefined || !Number.isInteger(ev.origin_id)) {
          throw new ValidationError("origin_id must be an integer");
        }
        const agent = nonEmpty(ev.agent ?? "", "agent", MAX_SENDER_LEN);
        // Same shared validator as PUT /presence (agent-hub#82 re-review
        // N3): status/session_id/capabilities/metadata were all previously
        // uncapped and untyped on this route too (1.5 MB metadata and a 1 MB
        // status both accepted; a non-string session_id stored as the
        // useless literal "[object Object]").
        const core = validatePresenceCore(ev);
        const timestampUtc = ev.timestamp_utc ? validateTimestampUtc(ev.timestamp_utc) : formatTimestampUtc();
        const protocolVersion = ev.protocol_version ? validateProtocolVersion(ev.protocol_version) : PROTOCOL_VERSION;
        const ttlSeconds = coerceOptionalInt(ev.ttl_seconds, "ttl_seconds", { min: 1, max: 86_400 });
        events.push({
          origin_id: ev.origin_id,
          timestamp_utc: timestampUtc,
          protocol_version: protocolVersion,
          agent,
          status: core.status,
          session_id: core.session_id || null,
          capabilities: core.capabilities,
          metadata: core.metadata,
          ttl_seconds: ttlSeconds ?? null,
        });
      } catch (err) {
        const reason = err instanceof Error ? err.message : String(err);
        preRejected.push({ origin_id: ev.origin_id ?? -1, reason });
      }
    }

    const result = await busLogStub(c.env).syncPushPresence(originHub, events);
    return { accepted: result.accepted, duplicates: result.duplicates, rejected: [...preRejected, ...result.rejected] };
  });
});

app.get("/sync/stats", async (c) => {
  return guarded(async () => {
    const identity = c.get("identity");
    requireRole(identity, "operator");
    return busLogStub(c.env).statsByOrigin();
  });
});

// --- Operator-only secret-map provenance; never export entries or tokens ------
app.get("/admin/tokens/manifest", (c) => guarded(async () => {
  requireRole(c.get("identity"), "operator");
  return { ...await tokenManifest(c.env.AGENT_BUS_TOKENS),
    build_version: BUILD_VERSION, hub_identity: cloudIdentity(c.env) };
}));

// --- MCP: supported REST-backed tools, with the same role/identity checks ------
app.all("/mcp", (c) => handleMcp(c.req.raw, async (request) => {
  mcpRestRequests.add(request);
  try { return await app.request(request, undefined, c.env); }
  finally { mcpRestRequests.delete(request); }
}));

// --- Deferred routes: return a clear 501 rather than a generic 404 -------------

const DEFERRED_ROUTES = [
  "/events",
  "/events/:agent_id",
  "/admin/control",
  "/admin/service",
  "/admin/service/control",
  "/channels/direct/:agent_id",
  "/channels/groups",
  "/channels/groups/:name/messages",
  "/channels/escalate",
  "/channels/summary",
  "/token-count",
  "/compact-context",
  "/session-summary",
  "/thread-summary",
  "/compact-thread",
  "/orchestrator-summary",
  "/tasks/:agent",
  "/subscriptions",
  "/subscriptions/:id",
  "/inventory",
  "/threads",
  "/threads/:id",
  "/threads/:id/join",
  "/threads/:id/leave",
  "/threads/:id/close",
  "/overdue-acks",
  "/dashboard",
  "/dashboard/data",
  "/support",
];
for (const path of DEFERRED_ROUTES) {
  app.all(path, (c) =>
    errorResponse(501, `${c.req.path} is not part of the agent-hub#79 cloud subset yet — see README.md`),
  );
}

app.notFound((c) => errorResponse(404, `no route for ${c.req.method} ${c.req.path}`));

export default app;
