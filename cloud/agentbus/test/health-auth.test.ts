import { describe, expect, it } from "vitest";
import { api, apiJson, AUTH_TOKEN, CLAUDE_TOKEN, CODEX_TOKEN } from "./helpers";

describe("GET /health", () => {
  it("is reachable with no Authorization header", async () => {
    const res = await api("/health", undefined, null);
    expect(res.status).toBe(200);
  });

  it("reveals no fleet data (no message/presence counts or agent names)", async () => {
    const { body } = await apiJson<Record<string, unknown>>("/health", undefined, null);
    expect(body).toMatchObject({
      ok: true,
      protocol_version: "1.0",
      storage_ready: true,
      runtime: "cloudflare-workers",
      codec: "json",
    });
    expect(body).not.toHaveProperty("pg_message_count");
    expect(body).not.toHaveProperty("pg_presence_count");
    expect(body).not.toHaveProperty("stream_length");
  });
});

describe("auth", () => {
  it("rejects every other route with no token", async () => {
    const res = await api("/presence", undefined, null);
    expect(res.status).toBe(401);
    const body = await res.json();
    expect(body).toMatchObject({ error: "unauthorized" });
  });

  it("rejects an invalid token", async () => {
    const res = await api("/presence", undefined, "not-a-real-token");
    expect(res.status).toBe(401);
  });

  it("accepts the shared AGENT_BUS_AUTH_TOKEN (parity fallback)", async () => {
    const res = await api("/presence", undefined, AUTH_TOKEN);
    expect(res.status).toBe(200);
  });

  it("accepts a per-agent token from AGENT_BUS_TOKENS", async () => {
    const res = await api("/presence", undefined, CLAUDE_TOKEN);
    expect(res.status).toBe(200);
  });

  it("accepts a second, distinct per-agent token", async () => {
    const res = await api("/presence", undefined, CODEX_TOKEN);
    expect(res.status).toBe(200);
  });
});

describe("deferred routes", () => {
  it("returns 501 (not 404) for a route outside the #79 subset", async () => {
    const res = await api("/dashboard");
    expect(res.status).toBe(501);
  });
});
