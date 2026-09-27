import { describe, expect, it } from "vitest";
import { apiJson, putJson } from "./helpers";

describe("PUT /presence/:agent", () => {
  it("sets presence and returns the Presence shape", async () => {
    const { status, body } = await putJson<Record<string, unknown>>("/presence/claude-presence-test", {
      status: "online",
      session_id: "session-1",
      capabilities: ["mcp", "http"],
      ttl_seconds: 300,
      metadata: { host: "asuspro13" },
    });
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

  it("rejects an empty agent", async () => {
    const { status } = await putJson("/presence/%20", { status: "online" });
    expect(status).toBe(400);
  });

  it("accepts agent-hub#79's network_context field", async () => {
    const { body } = await putJson<{ network_context?: string }>("/presence/roaming-agent", {
      status: "online",
      network_context: "offsite",
    });
    expect(body.network_context).toBe("offsite");
  });
});

describe("GET /presence", () => {
  it("lists live presence records", async () => {
    await putJson("/presence/list-test-agent", { status: "online", ttl_seconds: 300 });
    const { body } = await apiJson<Array<{ agent: string }>>("/presence");
    expect(body.some((p) => p.agent === "list-test-agent")).toBe(true);
  });
});

describe("GET /presence/history", () => {
  it("records every set_presence call, newest first", async () => {
    await putJson("/presence/history-test-agent", { status: "online" });
    await putJson("/presence/history-test-agent", { status: "busy" });
    const { body } = await apiJson<Array<{ agent: string; status: string }>>(
      "/presence/history?agent=history-test-agent",
    );
    expect(body.length).toBeGreaterThanOrEqual(2);
    expect(body[0]?.status).toBe("busy");
  });
});
