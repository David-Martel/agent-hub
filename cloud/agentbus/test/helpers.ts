import { env } from "cloudflare:workers";
import app from "../src/index";

/** Matches the `AGENT_BUS_AUTH_TOKEN` test binding in `vitest.config.ts`. */
export const AUTH_TOKEN = "test-shared-token";
export const CLAUDE_TOKEN = "test-token-claude";
export const CODEX_TOKEN = "test-token-codex";

export function authHeaders(token: string = AUTH_TOKEN): Record<string, string> {
  return { authorization: `Bearer ${token}`, "content-type": "application/json" };
}

/** Thin wrapper around Hono's `app.request()` bound to the test `env`. */
export async function api(
  path: string,
  init?: RequestInit,
  token: string | null = AUTH_TOKEN,
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
  token: string | null = AUTH_TOKEN,
): Promise<{ status: number; body: T }> {
  const res = await api(path, init, token);
  const body = (await res.json()) as T;
  return { status: res.status, body };
}

export function postJson<T = unknown>(path: string, payload: unknown, token: string | null = AUTH_TOKEN) {
  return apiJson<T>(path, { method: "POST", body: JSON.stringify(payload) }, token);
}

export function putJson<T = unknown>(path: string, payload: unknown, token: string | null = AUTH_TOKEN) {
  return apiJson<T>(path, { method: "PUT", body: JSON.stringify(payload) }, token);
}
