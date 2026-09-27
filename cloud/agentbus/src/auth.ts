/**
 * Bearer-token auth mirroring the Rust hub's `require_bearer_auth`: `/health`
 * is exempt, every other route requires `Authorization: Bearer <token>`.
 *
 * The cloud tier extends this (per agent-hub#79 §Auth) to support SEVERAL
 * tokens, each mapped to an agent/host identity for audit logging — the
 * Rust hub only supports a single shared token today. `AGENT_BUS_TOKENS` is
 * the preferred secret (JSON map); `AGENT_BUS_AUTH_TOKEN` (a single string)
 * is accepted for parity with the on-site hub's env var name and to keep a
 * one-token deployment simple.
 *
 * Cloudflare Access service tokens can be layered on later in front of this
 * Worker (verifying `Cf-Access-Jwt-Assertion`) without changing this
 * interface — `authenticate()` already returns an `Identity | null` that a
 * future Access-token branch could also populate.
 */

export interface Identity {
  token: string;
  agent: string;
  host?: string;
}

export interface AuthEnv {
  AGENT_BUS_TOKENS?: string;
  AGENT_BUS_AUTH_TOKEN?: string;
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
}

function parseTokenMap(json: string | undefined): Map<string, TokenMapEntry> {
  const map = new Map<string, TokenMapEntry>();
  if (!json) return map;
  try {
    const parsed = JSON.parse(json) as Record<string, TokenMapEntry>;
    for (const [token, entry] of Object.entries(parsed)) {
      if (entry && typeof entry.agent === "string") {
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
 * `AGENT_BUS_TOKENS` (preferred) and `AGENT_BUS_AUTH_TOKEN` (fallback).
 * Returns the resolved identity on success, `null` on missing/invalid token.
 */
export function authenticate(request: Request, env: AuthEnv): Identity | null {
  const header = request.headers.get("Authorization");
  if (!header?.startsWith("Bearer ")) return null;
  const token = header.slice("Bearer ".length);
  if (!token) return null;

  const tokenMap = parseTokenMap(env.AGENT_BUS_TOKENS);
  for (const [candidate, entry] of tokenMap) {
    if (timingSafeEqual(token, candidate)) {
      return { token, agent: entry.agent, host: entry.host };
    }
  }

  if (env.AGENT_BUS_AUTH_TOKEN && timingSafeEqual(token, env.AGENT_BUS_AUTH_TOKEN)) {
    return { token, agent: "unknown" };
  }

  return null;
}

export function unauthorizedResponse(): Response {
  return new Response(
    JSON.stringify({ error: "unauthorized", message: "missing or invalid bearer token" }),
    {
      status: 401,
      headers: { "content-type": "application/json", "www-authenticate": "Bearer" },
    },
  );
}
