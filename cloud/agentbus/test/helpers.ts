import { env } from "cloudflare:workers";
import app from "../src/index";

/** Shared-token dev fallback (`AGENT_BUS_DEV_ALLOW_SHARED_TOKEN=1` in
 * `vitest.config.ts`) — role `agent`, agent `dev-shared`. Kept ONLY for
 * `health-auth.test.ts` to exercise that specific path; every other test
 * uses an identity-bound role token below (agent-hub#82: the old shared
 * token could assert any `sender`/`agent`/`origin_hub` with no binding at
 * all, which is exactly what this PR fixes). */
export const AUTH_TOKEN = "test-shared-token";
export const CLAUDE_TOKEN = "test-token-claude";
export const CODEX_TOKEN = "test-token-codex";
/** Hub-role tokens: `hub: "asuspro13"` / `hub: "spark-0060"` respectively.
 * Two distinct hubs so cross-origin tests (H5, the (origin_hub, id) dedup
 * key) have two real identities to compare. */
export const HUB_A_TOKEN = "test-token-hub-a";
export const HUB_B_TOKEN = "test-token-hub-b";
export const HUB_A = "asuspro13";
export const HUB_B = "spark-0060";
export const OPERATOR_TOKEN = "test-token-operator";

/** Default caller identity for tests that don't care which agent they are. */
export const DEFAULT_TOKEN = CLAUDE_TOKEN;

export function authHeaders(token: string = DEFAULT_TOKEN): Record<string, string> {
  return { authorization: `Bearer ${token}`, "content-type": "application/json" };
}

/** Thin wrapper around Hono's `app.request()` bound to the test `env`. */
export async function api(
  path: string,
  init?: RequestInit,
  token: string | null = DEFAULT_TOKEN,
): Promise<Response> {
  const headers = new Headers(init?.headers);
  if (token !== null && !headers.has("authorization")) {
    headers.set("authorization", `Bearer ${token}`);
  }
  if (init?.body !== undefined && !headers.has("content-type")) {
    headers.set("content-type", "application/json");
  }
  return app.request(path, { ...init, headers }, env);
}

export async function apiJson<T = unknown>(
  path: string,
  init?: RequestInit,
  token: string | null = DEFAULT_TOKEN,
): Promise<{ status: number; body: T }> {
  const res = await api(path, init, token);
  const body = (await res.json()) as T;
  return { status: res.status, body };
}

export function postJson<T = unknown>(path: string, payload: unknown, token: string | null = DEFAULT_TOKEN) {
  return apiJson<T>(path, { method: "POST", body: JSON.stringify(payload) }, token);
}

export function putJson<T = unknown>(path: string, payload: unknown, token: string | null = DEFAULT_TOKEN) {
  return apiJson<T>(path, { method: "PUT", body: JSON.stringify(payload) }, token);
}
