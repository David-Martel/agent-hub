import { describe, expect, it } from "vitest";
import { apiJson, CLAUDE_TOKEN, HUB_A, HUB_A_TOKEN, HUB_B, HUB_B_TOKEN, putJson } from "./helpers";

// agent-hub#82 review M9: presence is now keyed by (hub-or-host, agent), not
// agent alone, so an arbitrary synthetic agent name (e.g.
// "claude-presence-test") must come from a token that's allowed to VOUCH for
// it — a hub-role token — rather than the default CLAUDE_TOKEN, which is
// bound to agent "claude" specifically.

describe("PUT /presence/:agent", () => {
  it("sets presence and returns the Presence shape", async () => {
    const { status, body } = await putJson<Record<string, unknown>>(
      "/presence/claude-presence-test",
      {
        status: "online",
        session_id: "session-1",
        capabilities: ["mcp", "http"],
        ttl_seconds: 300,
        metadata: { host: "asuspro13" },
      },
      HUB_A_TOKEN,
    );
    expect(status).toBe(200);
    expect(body).toMatchObject({
      agent: "claude-presence-test",
      status: "online",
      protocol_version: "1.0",
      session_id: "session-1",
      capabilities: ["mcp", "http"],
      ttl_seconds: 300,
    });
  });

  it("an agent-role token sets presence for its OWN agent name with no hub vouching needed", async () => {
    const { status, body } = await putJson<Record<string, unknown>>(
      "/presence/claude",
      { status: "online" },
      CLAUDE_TOKEN,
    );
    expect(status).toBe(200);
    expect(body).toMatchObject({ agent: "claude", status: "online" });
  });

  it("REJECTS an agent-role token setting presence for a DIFFERENT agent (403)", async () => {
    const { status } = await putJson("/presence/someone-else", { status: "online" }, CLAUDE_TOKEN);
    expect(status).toBe(403);
  });

  it("rejects an empty agent", async () => {
    const { status } = await putJson("/presence/%20", { status: "online" }, HUB_A_TOKEN);
    expect(status).toBe(400);
  });

  it("accepts agent-hub#79's network_context field", async () => {
    const { body } = await putJson<{ network_context?: string }>(
      "/presence/roaming-agent",
      { status: "online", network_context: "offsite" },
      HUB_A_TOKEN,
    );
    expect(body.network_context).toBe("offsite");
  });

  it("rejects presence metadata matching an obvious PHI pattern (review M7)", async () => {
    const { status, body } = await putJson<{ error: string }>(
      "/presence/phi-metadata-agent",
      { status: "online", metadata: { note: "patient name is on file" } },
      HUB_A_TOKEN,
    );
    expect(status).toBe(400);
    expect(body.error).toMatch(/PHI/);
  });
});

describe("GET /presence", () => {
  it("lists live presence records", async () => {
    await putJson("/presence/list-test-agent", { status: "online", ttl_seconds: 300 }, HUB_A_TOKEN);
    const { body } = await apiJson<Array<{ agent: string }>>("/presence");
    expect(body.some((p) => p.agent === "list-test-agent")).toBe(true);
  });

  it("two different hubs' same-named agent do not collide (review M9)", async () => {
    const agent = `dup-agent-${crypto.randomUUID()}`;
    await putJson(`/presence/${agent}`, { status: "online" }, HUB_A_TOKEN);
    await putJson(`/presence/${agent}`, { status: "busy" }, HUB_B_TOKEN);

    const { body } = await apiJson<Array<{ agent: string; status: string; origin_hub?: string }>>("/presence");
    const rows = body.filter((p) => p.agent === agent);
    expect(rows).toHaveLength(2);
    expect(rows.some((r) => r.origin_hub === HUB_A && r.status === "online")).toBe(true);
    expect(rows.some((r) => r.origin_hub === HUB_B && r.status === "busy")).toBe(true);
  });
});

describe("GET /presence/history", () => {
  it("records every set_presence call, newest first", async () => {
    await putJson("/presence/history-test-agent", { status: "online" }, HUB_A_TOKEN);
    await putJson("/presence/history-test-agent", { status: "busy" }, HUB_A_TOKEN);
    const { body } = await apiJson<Array<{ agent: string; status: string }>>(
      "/presence/history?agent=history-test-agent",
    );
    expect(body.length).toBeGreaterThanOrEqual(2);
    expect(body[0]?.status).toBe("busy");
  });
});
