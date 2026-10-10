/**
 * `ClaimDO` Durable Object retained from the agent-hub#79 authority proposal.
 * Cloud HTTP mutations are disabled; this direct implementation is not
 * the current on-site claims authority.
 *
 * One instance per resource (`env.CLAIM_DO.idFromName(normalizeResource(resource))`),
 * giving a single strongly-consistent writer per resource without any extra
 * locking: every method below runs synchronously against the DO's own SQLite
 * storage (`ctx.storage.sql`), and Durable Objects process one request at a
 * time, so two concurrent `claim()` calls on the same resource are
 * serialized by the runtime — the second sees the first's write.
 *
 * Semantics are a direct port of `agent-bus-core::channels`
 * (`claim_resource_with_options`, `renew_claim`, `release_claim`,
 * `resolve_claim`, `get_arbitration_state`, `recompute_claim_statuses`,
 * `suggest_reroute`) — see `claims-logic.ts` for the pure conflict/reroute
 * logic shared with the tests.
 *
 * Two intentional deviations from the Rust hub, both preserved for fidelity
 * rather than "fixed", and called out again in the PR / route table:
 *   - `renew()` on a resource/agent pair with no active claim throws a
 *     generic (500-mapped) error, matching `channels::renew_claim`'s
 *     `AgentBusError::Internal` — NOT the 400 you might expect.
 *   - `release()`/`resolve()` on a missing claim/resource throw a
 *     ValidationError (400-mapped), matching `AgentBusError::InvalidParams`.
 */

import { DurableObject } from "cloudflare:workers";
import { claimConflicts, effectiveScope, recomputeClaimStatuses, suggestReroute } from "./claims-logic";
import type { Env } from "./env";
import { formatTimestampUtc } from "./ids";
import type { ArbitrationState, OwnershipClaim, ResourceEvent, ResourceLeaseMode, ResourceScope } from "./types";
import { MAX_LEASE_TTL_SECONDS, MIN_LEASE_TTL_SECONDS, ValidationError } from "./validation";

/** Adds the index signature `sql.exec<T>()` requires without repeating it at every call site. */
type Row<T> = T & Record<string, SqlStorageValue>;

const SCHEMA = `
CREATE TABLE IF NOT EXISTS claims (
  agent TEXT PRIMARY KEY,
  resource TEXT NOT NULL,
  priority_argument TEXT NOT NULL,
  timestamp TEXT NOT NULL,
  status TEXT NOT NULL,
  mode TEXT NOT NULL,
  namespace TEXT,
  scope_kind TEXT,
  scope_path TEXT,
  repo_scopes TEXT NOT NULL DEFAULT '[]',
  thread_id TEXT,
  lease_ttl_seconds INTEGER NOT NULL,
  expires_at TEXT,
  scope TEXT NOT NULL,
  reroute_suggestion TEXT
);

CREATE TABLE IF NOT EXISTS resolution (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  winner TEXT NOT NULL,
  reason TEXT NOT NULL,
  resolved_by TEXT NOT NULL,
  resolved_at TEXT NOT NULL,
  expires_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  event TEXT NOT NULL,
  agent TEXT NOT NULL,
  resource TEXT NOT NULL,
  timestamp TEXT NOT NULL
);
`;

const CLAIM_TTL_SECS = 3600;
/** Mirrors the Rust hub's `CLAIM_TTL_SECS` reuse as the resolution-record TTL. */
const RESOLUTION_TTL_SECS = 3600;
/** MAXLEN parity with `agent_bus:resource_events:<resource>` (trimmed to ~1000). */
const EVENTS_MAXLEN = 1000;

interface ClaimRow {
  [key: string]: SqlStorageValue;
  agent: string;
  resource: string;
  priority_argument: string;
  timestamp: string;
  status: string;
  mode: string;
  namespace: string | null;
  scope_kind: string | null;
  scope_path: string | null;
  repo_scopes: string;
  thread_id: string | null;
  lease_ttl_seconds: number;
  expires_at: string | null;
  scope: string;
  reroute_suggestion: string | null;
}

function rowToClaim(row: ClaimRow): OwnershipClaim {
  const claim: OwnershipClaim = {
    resource: row.resource,
    agent: row.agent,
    priority_argument: row.priority_argument,
    timestamp: row.timestamp,
    status: row.status as OwnershipClaim["status"],
    mode: row.mode as ResourceLeaseMode,
    lease_ttl_seconds: row.lease_ttl_seconds,
    scope: row.scope as ResourceScope,
  };
  if (row.namespace !== null) claim.namespace = row.namespace;
  if (row.scope_kind !== null) claim.scope_kind = row.scope_kind;
  if (row.scope_path !== null) claim.scope_path = row.scope_path;
  const repoScopes = JSON.parse(row.repo_scopes) as string[];
  if (repoScopes.length > 0) claim.repo_scopes = repoScopes;
  if (row.thread_id !== null) claim.thread_id = row.thread_id;
  if (row.expires_at !== null) claim.expires_at = row.expires_at;
  if (row.reroute_suggestion !== null) claim.reroute_suggestion = JSON.parse(row.reroute_suggestion);
  return claim;
}

export interface ClaimRequestInput {
  resource: string;
  agent: string;
  priorityArgument: string;
  mode: ResourceLeaseMode;
  namespace?: string;
  scopeKind?: string;
  scopePath?: string;
  repoScopes?: string[];
  threadId?: string;
  leaseTtlSeconds?: number;
  scope?: ResourceScope;
}

export class ClaimDO extends DurableObject<Env> {
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

  /** True once this DO's tables have actually been created. Read-only
   * probes (`getState`, `listEvents`) use this to avoid minting a
   * persistent DO for a resource nobody has ever claimed (agent-hub#82
   * review M6: `GET /resource-events/:id` on an unclaimed resource used to
   * call `ensureSchema()` unconditionally). A plain `SELECT` against
   * `sqlite_master` does not itself create or persist anything. */
  private hasSchema(): boolean {
    if (this.initialized) return true;
    const row = this.sql
      .exec<Row<{ c: number }>>("SELECT count(*) as c FROM sqlite_master WHERE type = 'table' AND name = 'claims'")
      .toArray()[0];
    if ((row?.c ?? 0) > 0) {
      this.initialized = true;
      return true;
    }
    return false;
  }

  private pruneExpired(now: number): void {
    const expired = this.sql
      .exec<Row<{ agent: string }>>(
        "SELECT agent FROM claims WHERE expires_at IS NOT NULL AND expires_at <= ?",
        formatTimestampUtc(new Date(now)),
      )
      .toArray();
    if (expired.length > 0) {
      for (const row of expired) {
        this.sql.exec("DELETE FROM claims WHERE agent = ?", row.agent);
      }
    }
  }

  private loadActiveClaims(now = Date.now()): OwnershipClaim[] {
    this.ensureSchema();
    this.pruneExpired(now);
    const rows = this.sql.exec<ClaimRow>("SELECT * FROM claims ORDER BY timestamp ASC").toArray();
    return rows.map(rowToClaim);
  }

  private persistClaims(claims: OwnershipClaim[]): void {
    this.sql.exec("DELETE FROM claims");
    for (const claim of claims) {
      this.sql.exec(
        `INSERT INTO claims
          (agent, resource, priority_argument, timestamp, status, mode, namespace, scope_kind,
           scope_path, repo_scopes, thread_id, lease_ttl_seconds, expires_at, scope, reroute_suggestion)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        claim.agent,
        claim.resource,
        claim.priority_argument,
        claim.timestamp,
        claim.status,
        claim.mode,
        claim.namespace ?? null,
        claim.scope_kind ?? null,
        claim.scope_path ?? null,
        JSON.stringify(claim.repo_scopes ?? []),
        claim.thread_id ?? null,
        claim.lease_ttl_seconds,
        claim.expires_at ?? null,
        claim.scope,
        claim.reroute_suggestion ? JSON.stringify(claim.reroute_suggestion) : null,
      );
    }
  }

  private emitEvent(event: string, agent: string, resource: string): void {
    this.sql.exec(
      "INSERT INTO events (event, agent, resource, timestamp) VALUES (?, ?, ?, ?)",
      event,
      agent,
      resource,
      formatTimestampUtc(),
    );
    const count = this.sql.exec<Row<{ c: number }>>("SELECT COUNT(*) as c FROM events").toArray()[0]?.c ?? 0;
    if (count > EVENTS_MAXLEN) {
      this.sql.exec(
        "DELETE FROM events WHERE seq IN (SELECT seq FROM events ORDER BY seq ASC LIMIT ?)",
        count - EVENTS_MAXLEN,
      );
    }
  }

  listEvents(limit = 100): ResourceEvent[] {
    if (!this.hasSchema()) return [];
    const rows = this.sql
      .exec<Row<{ event: string; agent: string; resource: string; timestamp: string; seq: number }>>(
        "SELECT * FROM events ORDER BY seq DESC LIMIT ?",
        limit,
      )
      .toArray();
    return rows.map((r) => ({
      event: r.event,
      agent: r.agent,
      resource: r.resource,
      timestamp: r.timestamp,
      stream_id: String(r.seq),
    }));
  }

  claim(input: ClaimRequestInput, nowIso: string): OwnershipClaim {
    this.ensureSchema();
    if (!input.resource.trim()) throw new ValidationError("resource must not be empty");
    if (!input.agent.trim()) throw new ValidationError("agent must not be empty");
    if (input.mode === "shared_namespaced" && !(input.namespace ?? "").trim()) {
      throw new ValidationError("shared_namespaced claims require --namespace");
    }

    const now = Date.now();
    const leaseTtl = Math.min(
      Math.max(input.leaseTtlSeconds ?? CLAIM_TTL_SECS, MIN_LEASE_TTL_SECONDS),
      MAX_LEASE_TTL_SECONDS,
    );
    const expiresAt = formatTimestampUtc(new Date(now + leaseTtl * 1000));

    let claims = this.loadActiveClaims(now).filter((c) => c.agent !== input.agent);
    const resolvedScope = effectiveScope(input.resource, input.scope);
    const newClaim: OwnershipClaim = {
      resource: input.resource,
      agent: input.agent,
      priority_argument: input.priorityArgument,
      timestamp: nowIso,
      status: "pending",
      mode: input.mode,
      lease_ttl_seconds: leaseTtl,
      expires_at: expiresAt,
      scope: resolvedScope,
    };
    if (input.namespace) newClaim.namespace = input.namespace;
    if (input.scopeKind) newClaim.scope_kind = input.scopeKind;
    if (input.scopePath) newClaim.scope_path = input.scopePath;
    if (input.repoScopes && input.repoScopes.length > 0) newClaim.repo_scopes = input.repoScopes;
    if (input.threadId) newClaim.thread_id = input.threadId;
    claims.push(newClaim);

    recomputeClaimStatuses(claims);
    this.persistClaims(claims);

    const mine = claims.find((c) => c.agent === input.agent);
    if (!mine) throw new Error("claimed resource not found after persist");

    const otherClaimants = claims.filter((c) => c.agent !== input.agent).map((c) => c.agent);
    if (mine.status === "contested") {
      mine.reroute_suggestion = suggestReroute(input.resource, input.agent, otherClaimants);
    }

    this.emitEvent(mine.status === "contested" ? "contested" : "claimed", input.agent, input.resource);
    return mine;
  }

  renew(resource: string, agent: string, leaseTtlSeconds: number | undefined, nowIso: string): OwnershipClaim {
    this.ensureSchema();
    if (!agent.trim()) throw new ValidationError("agent must not be empty");
    if (!resource.trim()) throw new ValidationError("resource must not be empty");

    const now = Date.now();
    const claims = this.loadActiveClaims(now);
    const target = claims.find((c) => c.agent === agent);
    if (!target) {
      // Mirrors `channels::renew_claim`'s `AgentBusError::Internal` — an
      // intentional 500, not a 400. See the module docblock.
      throw new Error("no active claim found for agent on resource");
    }
    const ttl = Math.min(Math.max(leaseTtlSeconds ?? CLAIM_TTL_SECS, MIN_LEASE_TTL_SECONDS), MAX_LEASE_TTL_SECONDS);
    target.lease_ttl_seconds = ttl;
    target.timestamp = nowIso;
    target.expires_at = formatTimestampUtc(new Date(now + ttl * 1000));

    recomputeClaimStatuses(claims);
    this.persistClaims(claims);
    this.emitEvent("renewed", agent, resource);

    return claims.find((c) => c.agent === agent) ?? target;
  }

  release(resource: string, agent: string): ArbitrationState {
    this.ensureSchema();
    if (!agent.trim()) throw new ValidationError("agent must not be empty");
    if (!resource.trim()) throw new ValidationError("resource must not be empty");

    const claims = this.loadActiveClaims();
    const initialLen = claims.length;
    const remaining = claims.filter((c) => c.agent !== agent);
    if (remaining.length === initialLen) {
      throw new ValidationError("no active claim found for agent on resource");
    }
    recomputeClaimStatuses(remaining);
    this.persistClaims(remaining);
    this.emitEvent("released", agent, resource);
    return this.getState(resource);
  }

  resolve(resource: string, winner: string, reason: string, resolvedBy: string): ArbitrationState {
    this.ensureSchema();
    if (!winner.trim()) throw new ValidationError("winner must not be empty");
    if (!resource.trim()) throw new ValidationError("resource must not be empty");

    const claims = this.loadActiveClaims();
    if (claims.length === 0) {
      throw new ValidationError(`no claims found for resource '${resource}'`);
    }
    for (const claim of claims) {
      claim.status = claim.agent === winner ? "granted" : "review_assigned";
    }
    this.persistClaims(claims);

    const resolvedAt = formatTimestampUtc();
    const expiresAtMs = Date.now() + RESOLUTION_TTL_SECS * 1000;
    this.sql.exec(
      `INSERT INTO resolution (id, winner, reason, resolved_by, resolved_at, expires_at_ms)
       VALUES (1, ?, ?, ?, ?, ?)
       ON CONFLICT(id) DO UPDATE SET winner = excluded.winner, reason = excluded.reason,
         resolved_by = excluded.resolved_by, resolved_at = excluded.resolved_at,
         expires_at_ms = excluded.expires_at_ms`,
      winner,
      reason,
      resolvedBy,
      resolvedAt,
      expiresAtMs,
    );
    this.emitEvent("resolved", winner, resource);
    return this.getState(resource);
  }

  getState(resource: string): ArbitrationState {
    if (!this.hasSchema()) {
      return { resource, claims: [], winner: null, resolution_reason: null };
    }
    const claims = this.loadActiveClaims();
    const res = this.sql
      .exec<Row<{ winner: string; reason: string; expires_at_ms: number }>>(
        "SELECT winner, reason, expires_at_ms FROM resolution WHERE id = 1",
      )
      .toArray()[0];
    const resolutionLive = res && res.expires_at_ms > Date.now();
    return {
      resource,
      claims,
      winner: resolutionLive ? res.winner : null,
      resolution_reason: resolutionLive ? res.reason : null,
    };
  }

  override async fetch(): Promise<Response> {
    return new Response("ClaimDO Durable Object: RPC only", { status: 404 });
  }
}

// Re-exported so `claimConflicts` stays reachable for anyone importing this
// module for tests without pulling in `claims-logic` directly.
export { claimConflicts };
