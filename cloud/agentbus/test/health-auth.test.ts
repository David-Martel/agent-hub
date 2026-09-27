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

  it("warns when the shared AGENT_BUS_AUTH_TOKEN dev fallback is enabled (2026-09-27 token-rotation follow-up)", async () => {
    // This test binding always has AGENT_BUS_DEV_ALLOW_SHARED_TOKEN=1 (see
    // vitest.config.ts, needed by the "accepts the shared AGENT_BUS_AUTH_TOKEN"
    // test above), so /health must surface the warning unconditionally here.
    // A real production deploy should never set this flag at all, in which
    // case `warnings` is omitted entirely (see the `if` guard in
    // src/index.ts's /health handler) -- this test proves the mechanism
    // fires, not that it's off in this particular sandbox.
    const { body } = await apiJson<{ warnings?: string[] }>("/health", undefined, null);
    expect(body.warnings).toBeDefined();
    expect(body.warnings?.some((w) => w.includes("AGENT_BUS_DEV_ALLOW_SHARED_TOKEN"))).toBe(true);
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
