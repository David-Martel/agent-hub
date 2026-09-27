/**
 * Bearer-token auth mirroring the Rust hub's `require_bearer_auth`: `/health`
 * is exempt, every other route requires `Authorization: Bearer <token>`.
 *
 * agent-hub#82 security review (H1/H2/H5/L3): the original design resolved a
 * token to an `{agent, host}` identity but then never bound it to
 * anything — every actor field on every route (`sender`, `origin_host`,
 * `origin_hub`, the ack/knock/claim `agent`, ...) was taken verbatim from the
 * request body, making the whole auth layer decorative. This version:
 *
 *   - Adds a `role: "agent" | "hub" | "operator"` to every token-map entry.
 *     `hub`-role entries also carry a `hub` field — the ONLY source of
 *     `origin_hub` for `/sync/*` (see `bindOriginHubForSync` in
 *     `claims-logic.ts`... actually here, see below).
 *   - `agent`-role tokens are BOUND: a body value that disagrees with the
 *     token's own `agent`/`host` is a 403 (`bindAgent`/`bindOriginHost`
 *     below); a matching or omitted value is filled in from the token.
 *   - `hub`- and `operator`-role tokens may VOUCH for any `agent` value in
 *     the body without a 403. This is required for the on-site hub's future
 *     claims/messages proxy (SYNC-CONTRACT.md §6): a single hub-role token
 *     authenticates every proxied request, but each one names a different
 *     real on-site agent that the hub is relaying on behalf of, not
 *     impersonating.
 *   - `Identity.token` is REMOVED (review L3): it was stashed "for audit
 *     logging" but nothing ever logged it, and the first thing that did
 *     would have leaked a live bearer token.
 *   - The shared `AGENT_BUS_AUTH_TOKEN` fallback (an "unknown"-agent
 *     superuser under the old design) is now disabled by default and only
 *     active when `AGENT_BUS_DEV_ALLOW_SHARED_TOKEN=1` is explicitly set; it
 *     always resolves to the lowest-privilege role (`agent`), never
 *     `hub`/`operator`, so it can never reach `/sync/*` or `resolve`.
 *
 * Cloudflare Access service tokens can be layered on later in front of this
 * Worker (verifying `Cf-Access-Jwt-Assertion`) without changing this
 * interface — `authenticate()` already returns an `Identity | null` that a
 * future Access-token branch could also populate.
 */

import { ForbiddenError, ValidationError } from "./validation";

export type Role = "agent" | "hub" | "operator";

export interface Identity {
  agent: string;
  host?: string;
  role: Role;
  /** Only set (and only meaningful) for `role: "hub"` — the on-site hub
   * this token speaks for. The sole source of `origin_hub` on `/sync/*`. */
  hub?: string;
}

export interface AuthEnv {
  AGENT_BUS_TOKENS?: string;
  AGENT_BUS_AUTH_TOKEN?: string;
  /** Must be exactly `"1"` for `AGENT_BUS_AUTH_TOKEN` to be honored at all
   * (review H1/L5 — the shared-token fallback was an un-attributable
   * superuser by default). Intended for local `wrangler dev` / a
   * single-token smoke test, never production. */
  AGENT_BUS_DEV_ALLOW_SHARED_TOKEN?: string;
}

function timingSafeEqual(a: string, b: string): boolean {
  const enc = new TextEncoder();
  const ab = enc.encode(a);
  const bb = enc.encode(b);
  // Constant-time-ish comparison: always walk the longer buffer's length so
  // the loop count itself does not leak the matching prefix length.
  const len = Math.max(ab.length, bb.length);
  let diff = ab.length ^ bb.length;
  for (let i = 0; i < len; i++) {
    diff |= (ab[i] ?? 0) ^ (bb[i] ?? 0);
  }
  return diff === 0;
}

interface TokenMapEntry {
  agent: string;
  host?: string;
  role: Role;
  hub?: string;
}

const VALID_ROLES: readonly Role[] = ["agent", "hub", "operator"];
const VALID_ENTRY_KEYS: readonly string[] = ["agent", "host", "role", "hub"];

/**
 * Minimum accepted length for a bearer token, whether it's a key in
 * `AGENT_BUS_TOKENS` or the `AGENT_BUS_AUTH_TOKEN` shared fallback
 * (agent-hub#82 re-review N1). 32 is the review's own recommendation — long
 * enough that a short, guessable, or accidentally-truncated string (a stray
 * `"0"`, a copy-paste of just the token's prefix) can never authenticate.
 */
export const MIN_TOKEN_LEN = 32;

function isValidEntry(entry: unknown): entry is TokenMapEntry {
  if (!entry || typeof entry !== "object" || Array.isArray(entry)) return false;
  const e = entry as Record<string, unknown>;
  // Reject unknown keys outright (agent-hub#82 re-review N1) — a typo'd or
  // extra field (e.g. a stray `token` key left over from copy-pasting a
  // different config shape) is treated as a malformed entry, not silently
  // ignored.
  for (const key of Object.keys(e)) {
    if (!VALID_ENTRY_KEYS.includes(key)) return false;
  }
  if (typeof e.agent !== "string" || e.agent.length === 0) return false;
  if (e.host !== undefined && typeof e.host !== "string") return false;
  if (typeof e.role !== "string" || !(VALID_ROLES as string[]).includes(e.role)) return false;
  if (e.role === "hub" && (typeof e.hub !== "string" || e.hub.length === 0)) return false;
  if (e.hub !== undefined && typeof e.hub !== "string") return false;
  return true;
}

function parseTokenMap(json: string | undefined): Map<string, TokenMapEntry> {
  const map = new Map<string, TokenMapEntry>();
  if (!json) return map;
  try {
    const parsed: unknown = JSON.parse(json);
    // Fail closed on the WHOLE secret if the top-level shape is wrong
    // (agent-hub#82 re-review N1): `parseTokenMap` used to run
    // `Object.entries()` on whatever `JSON.parse` returned, including an
    // ARRAY — `Object.entries(["real-token-object"])` yields `[["0",
    // {...}]]`, so a secret accidentally (or maliciously) written as a JSON
    // array made the literal string `"0"` authenticate as that entry's
    // role, e.g. `Bearer 0` as operator, while the intended real token
    // matched nothing and got 401. A non-array, non-null object is the only
    // shape ever accepted; anything else yields an empty map (every token
    // gets 401 rather than some unintended subset working).
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      return map;
    }
    for (const [token, entry] of Object.entries(parsed as Record<string, unknown>)) {
      // Fail closed per-entry: a malformed entry (missing role, a hub-role
      // entry with no `hub`, an unknown key, ...) or a too-short token
      // string is simply not accepted, rather than silently downgrading its
      // privilege or throwing out of the request path.
      if (token.length >= MIN_TOKEN_LEN && isValidEntry(entry)) {
        map.set(token, entry);
      }
    }
  } catch {
    // Malformed secret: fail closed (no tokens accepted) rather than throw
    // out of the request path.
  }
  return map;
}

/**
 * Extract the bearer token from `Authorization`, then check it against
 * `AGENT_BUS_TOKENS` (preferred) and, only when explicitly enabled,
 * `AGENT_BUS_AUTH_TOKEN` (dev fallback). Returns the resolved identity on
 * success, `null` on missing/invalid token.
 */
export function authenticate(request: Request, env: AuthEnv): Identity | null {
  const header = request.headers.get("Authorization");
  if (!header?.startsWith("Bearer ")) return null;
  const token = header.slice("Bearer ".length);
  if (!token) return null;

  const tokenMap = parseTokenMap(env.AGENT_BUS_TOKENS);
  for (const [candidate, entry] of tokenMap) {
    if (timingSafeEqual(token, candidate)) {
      const identity: Identity = { agent: entry.agent, role: entry.role };
      if (entry.host) identity.host = entry.host;
      if (entry.role === "hub" && entry.hub) identity.hub = entry.hub;
      return identity;
    }
  }

  if (
    env.AGENT_BUS_DEV_ALLOW_SHARED_TOKEN === "1" &&
    env.AGENT_BUS_AUTH_TOKEN &&
    env.AGENT_BUS_AUTH_TOKEN.length >= MIN_TOKEN_LEN &&
    timingSafeEqual(token, env.AGENT_BUS_AUTH_TOKEN)
  ) {
    // Least privilege: never hub/operator, so this can't reach /sync/* or
    // resolve even when explicitly enabled for local dev.
    return { agent: "dev-shared", role: "agent" };
  }

  return null;
}

export function unauthorizedResponse(): Response {
  return new Response(
    JSON.stringify({ error: "unauthorized", message: "missing or invalid bearer token" }),
    {
      status: 401,
      headers: {
        "content-type": "application/json",
        "www-authenticate": "Bearer",
        "cache-control": "no-store",
        "x-content-type-options": "nosniff",
      },
    },
  );
}

export function requireRole(identity: Identity, ...allowed: Role[]): void {
  if (!allowed.includes(identity.role)) {
    throw new ForbiddenError(
      `this route requires role ${allowed.join(" or ")}; token has role '${identity.role}'`,
    );
  }
}

/**
 * Binds a body/path-supplied `agent`-shaped value to the caller's identity.
 * - `role: "agent"` tokens are bound: a supplied value that disagrees with
 *   `identity.agent` is a 403; a matching or omitted value resolves to
 *   `identity.agent`.
 * - `role: "hub" | "operator"` tokens may vouch for any non-empty value.
 */
export function bindAgent(identity: Identity, requested: string | undefined, fieldName = "agent"): string {
  const trimmed = requested?.trim();
  if (identity.role === "agent") {
    if (trimmed && trimmed !== identity.agent) {
      throw new ForbiddenError(
        `${fieldName} '${trimmed}' does not match the bearer token's identity '${identity.agent}'`,
      );
    }
    return identity.agent;
  }
  if (!trimmed) throw new ValidationError(`${fieldName} must not be empty`);
  return trimmed;
}

/** Same binding rule as `bindAgent`, but optional (used for fields like
 * `resolved_by` that are informational, not an ownership boundary). */
export function bindOptionalAgent(identity: Identity, requested: string | undefined): string {
  const trimmed = requested?.trim();
  if (identity.role === "agent") {
    if (trimmed && trimmed !== identity.agent) {
      throw new ForbiddenError(
        `agent '${trimmed}' does not match the bearer token's identity '${identity.agent}'`,
      );
    }
    return identity.agent;
  }
  return trimmed || identity.agent;
}

/** Binds `origin_host`: an `agent`-role token with a configured `host` must
 * match or omit it; a token with no configured host trusts the caller
 * (best-effort — nothing to compare against). Hub/operator tokens may
 * assert any host (they're relaying on behalf of a real on-site machine). */
export function bindOriginHost(identity: Identity, requested: string | undefined): string | undefined {
  if (identity.role === "agent" && identity.host) {
    if (requested && requested !== identity.host) {
      throw new ForbiddenError(
        `origin_host '${requested}' does not match the bearer token's host '${identity.host}'`,
      );
    }
    return identity.host;
  }
  return requested;
}

/** Binds `origin_hub` for a direct (non-sync) write: only a hub-role token
 * may assert a value other than the cloud tier's own identity (review H5 —
 * a spoofed `origin_hub` let one origin suppress another's `/sync/pull
 * ?exclude_origin=` view of its own writes). */
export function bindOriginHubForDirectWrite(
  identity: Identity,
  requested: string | undefined,
  cloudIdentity: string,
): string {
  if (identity.role === "hub" && identity.hub) {
    if (requested && requested !== identity.hub) {
      throw new ForbiddenError(`origin_hub '${requested}' does not match the bearer token's hub '${identity.hub}'`);
    }
    return identity.hub;
  }
  if (requested && requested !== cloudIdentity) {
    throw new ForbiddenError("origin_hub may only be asserted by a hub-role token");
  }
  return cloudIdentity;
}

/** `/sync/*` routes require a hub-role token; `origin_hub` for the whole
 * request always comes from `identity.hub`, never the body (review H5). */
export function requireHubIdentity(identity: Identity): string {
  requireRole(identity, "hub");
  if (!identity.hub) {
    // Defensive: parseTokenMap already rejects a hub-role entry without a
    // `hub` field, so this should be unreachable.
    throw new ForbiddenError("hub-role token is missing its hub identity");
  }
  return identity.hub;
}
