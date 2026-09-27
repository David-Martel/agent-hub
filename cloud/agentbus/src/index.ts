/**
 * agentbus.dtmventures.com — the off-site Cloudflare tier of agent-bus
 * (agent-hub#79). REST surface mirroring the subset of the Rust hub's HTTP
 * API listed in the issue: send/read/inbox/presence/claims/ack/health, plus
 * the new `/sync/push` and `/sync/pull` hub<->cloud replication endpoints.
 *
 * See `cloud/agentbus/README.md` for the deploy checklist and
 * `cloud/agentbus/SYNC-CONTRACT.md` for the contract the Rust-side sync
 * client (a later PR) must honor so the on-site hub is never adversely
 * affected by this tier's existence or reachability.
 */

import { Hono } from "hono";
import { authenticate, unauthorizedResponse, type AuthEnv, type Identity } from "./auth";
import type { BusLog, InsertMessageInput } from "./do-buslog";
import type { ClaimDO } from "./do-claim";
import { formatTimestampUtc, uuidv7 } from "./ids";
import {
  autoFitSchema,
  containsObviousPhi,
  enforceSchemaForTransport,
  nonEmpty,
  ValidationError,
  validateMessageSchema,
  validatePriority,
  validateSensitivity,
} from "./validation";
import { parseLeaseMode, parseResourceScope } from "./claims-logic";
import type { Health, JsonValue } from "./types";

export { BusLog } from "./do-buslog";
export { ClaimDO } from "./do-claim";

export interface Env extends AuthEnv {
  BUS_LOG: DurableObjectNamespace<BusLog>;
  CLAIM_DO: DurableObjectNamespace<ClaimDO>;
  /** Free-form identity string surfaced in `/health.hub_identity`. Defaults to `"cloud"`. */
  HUB_IDENTITY?: string;
}

const PROTOCOL_VERSION = "1.0";
const BUILD_VERSION = "agentbus-cloud@0.1.0";
const DEFAULT_SINCE_MINUTES = 60;
const MAX_HISTORY_MINUTES = 10_080; // 7 days, mirrors agent_bus_core::models::MAX_HISTORY_MINUTES
const DEFAULT_LIMIT = 50;

function busLogStub(env: Env) {
  const id = env.BUS_LOG.idFromName("global");
  return env.BUS_LOG.get(id);
}

function normalizeResource(resource: string): string {
  return resource.replace(/\\/g, "/");
}

function claimStub(env: Env, resource: string) {
  const id = env.CLAIM_DO.idFromName(normalizeResource(resource));
  return env.CLAIM_DO.get(id);
}

function errorResponse(status: number, message: string): Response {
  return new Response(JSON.stringify({ error: message }), {
    status,
    headers: { "content-type": "application/json" },
  });
}

function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json" },
  });
}

/** Runs `fn`, mapping a `ValidationError` to 400 and anything else to 500 —
 * mirrors `classify_core_error` in the Rust HTTP layer (bad input is a
 * client error; everything else is a server fault). */
async function guarded(fn: () => unknown | Promise<unknown>): Promise<Response> {
  try {
    const result = await fn();
    return json(result);
  } catch (err) {
    // A `ValidationError` thrown inside a Durable Object RPC method crosses
    // the DO<->Worker boundary via structured-clone-like serialization, which
    // does not preserve the custom subclass's prototype chain — `.name` (set
    // explicitly in the constructor) survives, but `instanceof` does not.
    // Check both so validation errors are classified the same way whether
    // they were thrown in-process (route-level validation) or inside a DO.
    const isValidationError =
      err instanceof ValidationError || (err instanceof Error && err.name === "ValidationError");
    if (isValidationError) {
      return errorResponse(400, (err as Error).message);
    }
    const message = err instanceof Error ? err.message : String(err);
    return errorResponse(500, message);
  }
}

type Bindings = { Bindings: Env; Variables: { identity: Identity } };
const app = new Hono<Bindings>();

// --- Auth middleware: every route except /health ----------------------------
//
// The resolved `Identity` (agent/host behind the bearer token) is stashed on
// the context for audit logging by individual handlers; none currently log
// it, but the plumbing is here so adding an audit trail later touches one
// place instead of every route.

app.use("*", async (c, next) => {
  if (c.req.path === "/health") return next();
  const identity = authenticate(c.req.raw, c.env);
  if (!identity) return unauthorizedResponse();
  c.set("identity", identity);
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
    hub_identity: c.env.HUB_IDENTITY ?? "cloud",
  };
  return json(health);
});

// --- POST /messages, GET /messages -----------------------------------------

interface SendBody {
  sender?: string;
  recipient?: string;
  topic?: string;
  body?: string;
  thread_id?: string;
  tags?: string[];
  priority?: string;
  request_ack?: boolean;
  reply_to?: string;
  metadata?: JsonValue;
  schema?: string;
  client_msg_id?: string;
  origin_host?: string;
  origin_hub?: string;
  hlc?: string;
  sensitivity?: string;
}

function buildValidatedMessage(req: SendBody, originHubDefault: string): InsertMessageInput {
  const priority = req.priority ?? "normal";
  validatePriority(priority);
  const sender = nonEmpty(req.sender ?? "", "sender");
  const recipient = nonEmpty(req.recipient ?? "", "recipient");
  const topic = nonEmpty(req.topic ?? "", "topic");
  const body = nonEmpty(req.body ?? "", "body");

  const effectiveSchema = enforceSchemaForTransport(req.schema, topic);
  const fittedBody = autoFitSchema(body, effectiveSchema);
  validateMessageSchema(fittedBody, effectiveSchema);

  const sensitivity = validateSensitivity(req.sensitivity);
  if (sensitivity === "no-offsite") {
    throw new ValidationError(
      "sensitivity=no-offsite messages are never accepted by the cloud tier (post them to the on-site hub only)",
    );
  }
  if (containsObviousPhi(fittedBody)) {
    throw new ValidationError(
      "body matches an obvious PHI pattern (SSN/MRN/DOB/patient-identifier) and was rejected; the bus must never carry PHI",
    );
  }

  const input: InsertMessageInput = {
    id: uuidv7(),
    timestamp_utc: formatTimestampUtc(),
    protocol_version: PROTOCOL_VERSION,
    from: sender,
    to: recipient,
    topic,
    body: fittedBody,
    tags: req.tags ?? [],
    priority,
    request_ack: req.request_ack ?? false,
    metadata: req.metadata ?? {},
    origin_hub: req.origin_hub ?? originHubDefault,
  };
  if (req.thread_id) input.thread_id = req.thread_id;
  if (req.reply_to) input.reply_to = req.reply_to;
  if (req.client_msg_id) input.client_msg_id = req.client_msg_id;
  if (req.origin_host) input.origin_host = req.origin_host;
  if (req.hlc) input.hlc = req.hlc;
  return input;
}

app.post("/messages", async (c) => {
  const body = await c.req.json<SendBody>().catch(() => ({}) as SendBody);
  return guarded(async () => {
    const input = buildValidatedMessage(body, c.env.HUB_IDENTITY ?? "cloud");
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
    const since = Math.min(Number(q.since ?? DEFAULT_SINCE_MINUTES), MAX_HISTORY_MINUTES);
    const limit = Math.min(Math.max(Number(q.limit ?? DEFAULT_LIMIT), 1), 500);
    const broadcast = q.broadcast === undefined ? true : q.broadcast === "true";
    const tags = new URL(c.req.url).searchParams.getAll("tag");
    const excerpt = q.excerpt ? Number(q.excerpt) : undefined;

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
    if (excerpt !== undefined && Number.isFinite(excerpt)) {
      msgs = msgs.map((m) => ({ ...m, body: m.body.length > excerpt ? `${m.body.slice(0, excerpt)}…` : m.body }));
    }
    return msgs;
  });
});

// --- POST /messages/batch ---------------------------------------------------

app.post("/messages/batch", async (c) => {
  const body = await c.req.json<{ messages?: SendBody[] }>().catch(() => ({}) as { messages?: SendBody[] });
  return guarded(async () => {
    const messages = body.messages ?? [];
    if (messages.length === 0) throw new ValidationError("messages array must not be empty");
    if (messages.length > 100) throw new ValidationError("batch size limit is 100 messages");

    const stub = busLogStub(c.env);
    const ids: string[] = [];
    for (const [idx, item] of messages.entries()) {
      let input: InsertMessageInput;
      try {
        input = buildValidatedMessage(item, (c.env.HUB_IDENTITY ?? "cloud"));
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        throw new ValidationError(`item ${idx}: ${msg}`);
      }
      const { message } = await stub.insertMessage(input);
      ids.push(message.id);
    }
    return { ids, count: ids.length };
  });
});

// --- POST /messages/:id/ack --------------------------------------------------

app.post("/messages/:id/ack", async (c) => {
  const messageId = c.req.param("id").trim();
  const body = await c.req.json<{ agent?: string; body?: string }>().catch(() => ({} as { agent?: string; body?: string }));
  return guarded(async () => {
    const agent = (body.agent ?? "").trim();
    if (!agent) throw new ValidationError("agent must not be empty");
    if (!messageId) throw new ValidationError("message id must not be empty");
    const ackBody = body.body ?? "ack";

    const stub = busLogStub(c.env);
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
      origin_hub: c.env.HUB_IDENTITY ?? "cloud",
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
    const since = Math.min(body.since ?? DEFAULT_SINCE_MINUTES, MAX_HISTORY_MINUTES);
    const limit = Math.min(Math.max(body.limit ?? DEFAULT_LIMIT, 1), 500);
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
    const agent = (body.agent ?? "").trim();
    if (!agent) throw new ValidationError("agent must not be empty");
    const ids = body.message_ids ?? [];
    if (ids.length === 0) throw new ValidationError("message_ids must not be empty");
    if (ids.length > 100) throw new ValidationError("batch ack limit is 100 message IDs");
    const ackBody = body.body ?? "ack";

    const stub = busLogStub(c.env);
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
        origin_hub: (c.env.HUB_IDENTITY ?? "cloud"),
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
    .json<{ sender?: string; recipient?: string; body?: string; thread_id?: string; tags?: string[]; request_ack?: boolean }>()
    .catch(() => ({} as { sender?: string; recipient?: string; body?: string; thread_id?: string; tags?: string[]; request_ack?: boolean }));
  return guarded(async () => {
    const sender = (body.sender ?? "").trim();
    const recipient = (body.recipient ?? "").trim();
    if (!sender) throw new ValidationError("sender must not be empty");
    if (!recipient) throw new ValidationError("recipient must not be empty");
    const knockBody = body.body ?? "check the bus";
    if (!knockBody.trim()) throw new ValidationError("body must not be empty");
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
      tags: body.tags ?? [],
      priority: "urgent",
      request_ack: requestAck,
      metadata: {
        knock: true,
        delivery_hint: "sse",
        expected_response_kind: requestAck ? "ack" : "status",
      },
      origin_hub: (c.env.HUB_IDENTITY ?? "cloud"),
    };
    if (body.thread_id) input.thread_id = body.thread_id;
    const { message } = await stub.insertMessage(input);
    return message;
  });
});

// --- PUT /presence/:agent, GET /presence, GET /presence/history ----------------

app.put("/presence/:agent", async (c) => {
  const agent = c.req.param("agent").trim();
  const body = await c.req
    .json<{ status?: string; session_id?: string; capabilities?: string[]; ttl_seconds?: number; metadata?: JsonValue; network_context?: string }>()
    .catch(() => ({} as { status?: string; session_id?: string; capabilities?: string[]; ttl_seconds?: number; metadata?: JsonValue; network_context?: string }));
  return guarded(async () => {
    if (!agent) throw new ValidationError("agent must not be empty");
    const ttl = Math.min(Math.max(body.ttl_seconds ?? 180, 1), 86_400);
    const stub = busLogStub(c.env);
    return stub.setPresence({
      agent,
      status: body.status ?? "online",
      protocol_version: PROTOCOL_VERSION,
      timestamp_utc: formatTimestampUtc(),
      session_id: body.session_id ?? "",
      capabilities: body.capabilities ?? [],
      metadata: body.metadata ?? {},
      ttl_seconds: ttl,
      network_context: body.network_context as never,
    });
  });
});

app.get("/presence", async (c) => {
  return guarded(async () => busLogStub(c.env).listPresence());
});

app.get("/presence/history", async (c) => {
  return guarded(async () => {
    const q = c.req.query();
    const since = Math.min(Number(q.since ?? DEFAULT_SINCE_MINUTES), MAX_HISTORY_MINUTES);
    const limit = Math.min(Math.max(Number(q.limit ?? DEFAULT_LIMIT), 1), 500);
    return busLogStub(c.env).listPresenceHistory(q.agent, Date.now() - since * 60_000, limit);
  });
});

// --- GET /notifications/:agent_id (inbox), GET /pending-acks -------------------

app.get("/notifications/:agent_id", async (c) => {
  const agentId = c.req.param("agent_id");
  return guarded(async () => {
    const q = c.req.query();
    const history = Math.min(Number(q.history ?? 20), 500);
    return busLogStub(c.env).listNotifications(agentId, q.since_id, history);
  });
});

app.get("/pending-acks", async (c) => {
  return guarded(async () => busLogStub(c.env).listPendingAcks(c.req.query("agent")));
});

// --- Claims: POST/GET /channels/arbitrate/:resource, resolve/renew/release -----

app.post("/channels/arbitrate/:resource", async (c) => {
  const resource = c.req.param("resource");
  const body = await c.req
    .json<{
      agent?: string;
      priority_argument?: string;
      mode?: string;
      namespace?: string;
      scope_kind?: string;
      scope_path?: string;
      repo_scopes?: string[];
      thread_id?: string;
      lease_ttl_seconds?: number;
      scope?: string;
    }>()
    .catch(() => ({} as {
      agent?: string;
      priority_argument?: string;
      mode?: string;
      namespace?: string;
      scope_kind?: string;
      scope_path?: string;
      repo_scopes?: string[];
      thread_id?: string;
      lease_ttl_seconds?: number;
      scope?: string;
    }));
  return guarded(async () => {
    const agent = (body.agent ?? "").trim();
    if (!agent) throw new ValidationError("agent must not be empty");
    const mode = parseLeaseMode(body.mode ?? "exclusive");
    const scope = body.scope ? parseResourceScope(body.scope) : undefined;
    const stub = claimStub(c.env, resource);
    return stub.claim(
      {
        resource,
        agent,
        priorityArgument: body.priority_argument ?? "first-edit required",
        mode,
        namespace: body.namespace,
        scopeKind: body.scope_kind,
        scopePath: body.scope_path,
        repoScopes: body.repo_scopes,
        threadId: body.thread_id,
        leaseTtlSeconds: body.lease_ttl_seconds ?? 3600,
        scope,
      },
      formatTimestampUtc(),
    );
  });
});

app.get("/channels/arbitrate/:resource", async (c) => {
  const resource = c.req.param("resource");
  return guarded(async () => claimStub(c.env, resource).getState(resource));
});

app.put("/channels/arbitrate/:resource/resolve", async (c) => {
  const resource = c.req.param("resource");
  const body = await c.req.json<{ winner?: string; reason?: string; resolved_by?: string }>().catch(() => ({} as { winner?: string; reason?: string; resolved_by?: string }));
  return guarded(async () => {
    const winner = (body.winner ?? "").trim();
    if (!winner) throw new ValidationError("winner must not be empty");
    return claimStub(c.env, resource).resolve(
      resource,
      winner,
      body.reason ?? "resolved by orchestrator",
      body.resolved_by ?? "orchestrator",
    );
  });
});

app.post("/channels/arbitrate/:resource/renew", async (c) => {
  const resource = c.req.param("resource");
  const body = await c.req.json<{ agent?: string; lease_ttl_seconds?: number }>().catch(() => ({} as { agent?: string; lease_ttl_seconds?: number }));
  return guarded(async () => {
    const agent = (body.agent ?? "").trim();
    if (!agent) throw new ValidationError("agent must not be empty");
    return claimStub(c.env, resource).renew(resource, agent, body.lease_ttl_seconds, formatTimestampUtc());
  });
});

app.post("/channels/arbitrate/:resource/release", async (c) => {
  const resource = c.req.param("resource");
  const body = await c.req.json<{ agent?: string }>().catch(() => ({} as { agent?: string }));
  return guarded(async () => {
    const agent = (body.agent ?? "").trim();
    if (!agent) throw new ValidationError("agent must not be empty");
    return claimStub(c.env, resource).release(resource, agent);
  });
});

app.get("/resource-events/:resource_id", async (c) => {
  const resource = c.req.param("resource_id");
  return guarded(async () => {
    const limit = Math.min(Number(c.req.query("limit") ?? 100), 1000);
    return claimStub(c.env, resource).listEvents(limit);
  });
});

// --- Sync (agent-hub#79): hub<->cloud replication ------------------------------

interface SyncPushMessageInput extends SendBody {
  id?: string;
  timestamp_utc?: string;
  protocol_version?: string;
}

app.post("/sync/push", async (c) => {
  const body = await c.req
    .json<{ origin_hub?: string; messages?: SyncPushMessageInput[] }>()
    .catch(() => ({} as { origin_hub?: string; messages?: SyncPushMessageInput[] }));
  return guarded(async () => {
    const originHub = (body.origin_hub ?? "").trim();
    if (!originHub) throw new ValidationError("origin_hub must not be empty");
    const messages = body.messages ?? [];
    if (messages.length > 500) throw new ValidationError("sync push batch limit is 500 messages");

    const items: InsertMessageInput[] = messages.map((m) => ({
      id: m.id && m.id.trim() ? m.id : uuidv7(),
      timestamp_utc: m.timestamp_utc ?? formatTimestampUtc(),
      protocol_version: m.protocol_version ?? PROTOCOL_VERSION,
      from: m.sender ?? "",
      to: m.recipient ?? "",
      topic: m.topic ?? "",
      body: m.body ?? "",
      thread_id: m.thread_id,
      tags: m.tags ?? [],
      priority: m.priority ?? "normal",
      request_ack: m.request_ack ?? false,
      reply_to: m.reply_to,
      metadata: m.metadata ?? {},
      client_msg_id: m.client_msg_id,
      origin_host: m.origin_host,
      origin_hub: m.origin_hub ?? originHub,
      hlc: m.hlc,
      sensitivity: m.sensitivity,
    }));

    return busLogStub(c.env).syncPush(originHub, items);
  });
});

app.get("/sync/pull", async (c) => {
  return guarded(async () => {
    const since = Math.max(Number(c.req.query("since") ?? 0), 0);
    const excludeOrigin = c.req.query("exclude_origin") || undefined;
    const limit = Math.min(Math.max(Number(c.req.query("limit") ?? 200), 1), 1000);
    return busLogStub(c.env).syncPull(since, excludeOrigin, limit);
  });
});

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
  "/mcp",
];
for (const path of DEFERRED_ROUTES) {
  app.all(path, (c) =>
    errorResponse(501, `${c.req.path} is not part of the agent-hub#79 cloud subset yet — see README.md`),
  );
}

app.notFound((c) => errorResponse(404, `no route for ${c.req.method} ${c.req.path}`));

export default app;
