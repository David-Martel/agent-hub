import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import app from "../src/index";
import { handleMcp } from "../src/mcp";
import { api, apiJson, postJson, CLAUDE_TOKEN, CODEX_TOKEN, HUB_A_TOKEN, OPERATOR_TOKEN } from "./helpers";

type Rpc = { id: unknown; result?: { content?: Array<{ type: string; text: string }>; isError?: boolean; [key: string]: unknown }; error?: { code: number; message: string } };
async function rpc(method: string, params: unknown = {}, token: string | null = CLAUDE_TOKEN) {
  return postJson<Rpc>("/mcp", { jsonrpc: "2.0", id: 1, method, params }, token);
}
async function call(name: string, args: unknown = {}, token: string = CLAUDE_TOKEN) {
  return rpc("tools/call", { name, arguments: args }, token);
}
function value<T = Record<string, unknown>>(response: { status: number; body: Rpc }): T {
  expect(response.status).toBe(200);
  expect(response.body.error).toBeUndefined();
  expect(response.body.result?.isError).toBe(false);
  expect(response.body.result?.content?.[0]?.type).toBe("text");
  return JSON.parse(response.body.result!.content![0]!.text) as T;
}
function rejected(response: { status: number; body: Rpc }, status: number) {
  expect(response.status).toBe(200);
  expect(response.body.result?.isError).toBe(true);
  expect(JSON.parse(response.body.result!.content![0]!.text)).toEqual({ error: "cloud tool request rejected", status });
}

describe("authenticated cloud MCP subset", () => {
  it("initializes, negotiates older versions and accepts initialized notifications", async () => {
    for (const version of ["2024-11-05", "2025-06-18", "future-unknown"]) {
      const response = await rpc("initialize", { protocolVersion: version, capabilities: {}, clientInfo: { name: "native-test", version: "1" } });
      expect(response.body.result).toMatchObject({ protocolVersion: version === "future-unknown" ? "2025-11-25" : version,
        capabilities: { tools: { listChanged: false } }, serverInfo: { name: "agentbus-cloud", version: "0.2.0" } });
    }
    const initialized = await api("/mcp", { method: "POST", body: JSON.stringify({ jsonrpc: "2.0", method: "notifications/initialized" }) });
    expect(initialized.status).toBe(202);
    expect(await initialized.text()).toBe("");
  });

  it("advertises only eight implemented native-named tools and constrained schemas", async () => {
    const response = await rpc("tools/list");
    const tools = response.body.result!.tools as Array<{ name: string; inputSchema: Record<string, unknown> }>;
    expect(tools.map((entry) => entry.name)).toEqual(["bus_health", "post_message", "list_messages", "ack_message", "set_presence", "list_presence", "list_presence_history", "knock_agent"]);
    expect(tools.every((entry) => entry.inputSchema.additionalProperties === false)).toBe(true);
    expect(tools.find((entry) => entry.name === "post_message")!.inputSchema.required).toEqual(["sender", "recipient", "topic", "body"]);
    expect(tools.find((entry) => entry.name === "list_messages")!.inputSchema.properties).toMatchObject({ since_minutes: { minimum: 1, maximum: 10080 }, limit: { maximum: 500 } });
  });

  it("serves direct native tools/call without handshake and returns JSON in content[0].text", async () => {
    const health = value(await call("bus_health"));
    expect(health).toMatchObject({ ok: true, runtime: "cloudflare-workers", database_ok: null });
    expect(health).not.toHaveProperty("backend"); // no invented on-site authority
  });

  it("posts and reads real DO messages with all filter mappings and defaults", async () => {
    const repo = `mcp-${crypto.randomUUID()}`;
    const message = value(await call("post_message", { sender: "claude", recipient: "codex", topic: "status", body: "mcp durable body", tags: [`repo:${repo}`, "session:mcp", "pick"], thread_id: repo, request_ack: true }));
    expect(message).toMatchObject({ from: "claude", to: "codex", body: "mcp durable body", origin_hub: "cloud", request_ack: true });
    const rows = value<Array<{ id: string }>>(await call("list_messages", { agent: "codex", sender: "claude", repo, session: "mcp", tag: ["pick"], thread_id: repo, topic: "status", include_broadcast: false, limit: 1 }));
    expect(rows.map((row) => row.id)).toEqual([message.id]);
    const ack = value(await call("ack_message", { agent: "codex", message_id: message.id }, CODEX_TOKEN));
    expect(ack).toMatchObject({ ack_sent: true, acked_message_id: message.id });
  });

  it("preserves recipient-only ack authorization and actor binding", async () => {
    const message = value(await call("post_message", { sender: "claude", recipient: "different-agent", topic: "status", body: "not your ack", request_ack: true }));
    rejected(await call("ack_message", { agent: "claude", message_id: message.id }), 403);
    rejected(await call("ack_message", { agent: "different-agent", message_id: message.id }), 403);
  });

  it.each(["post_message", "knock_agent"])("binds %s sender and retains hub/operator vouching", async (name) => {
    const args = { sender: "codex", recipient: "claude", ...(name === "post_message" ? { topic: "status", body: "role test" } : {}) };
    rejected(await call(name, args), 403);
    for (const token of [HUB_A_TOKEN, OPERATOR_TOKEN]) {
      expect(value(await call(name, args, token)).from).toBe("codex");
    }
  });

  it("preserves knock urgent defaults and real durable message shape", async () => {
    const knock = value(await call("knock_agent", { sender: "claude", recipient: "codex" }));
    expect(knock).toMatchObject({ from: "claude", to: "codex", topic: "knock", priority: "urgent", request_ack: true, body: "check the bus" });
  });

  it("binds presence writes and lists real current and historical presence", async () => {
    rejected(await call("set_presence", { agent: "codex" }), 403);
    const presence = value(await call("set_presence", { agent: "claude", capabilities: ["mcp"], ttl_seconds: 60, session_id: "mcp-role" }));
    expect(presence).toMatchObject({ agent: "claude", status: "online", capabilities: ["mcp"] });
    expect(value<Array<{ agent: string }>>(await call("list_presence")).some((entry) => entry.agent === "claude")).toBe(true);
    expect(value<Array<{ session_id: string }>>(await call("list_presence_history", { agent: "claude" })).some((entry) => entry.session_id === "mcp-role")).toBe(true);
    for (const token of [HUB_A_TOKEN, OPERATOR_TOKEN]) expect(value(await call("set_presence", { agent: "relayed-agent" }, token)).agent).toBe("relayed-agent");
  });

  it("allows privileged recipient ack without giving agents that privilege", async () => {
    for (const token of [HUB_A_TOKEN, OPERATOR_TOKEN]) {
      const sent = value(await call("post_message", { sender: "claude", recipient: "relayed-agent", topic: "status", body: "relay ack" }));
      expect(value(await call("ack_message", { agent: "relayed-agent", message_id: sent.id }, token)).ack_sent).toBe(true);
    }
  });

  it.each([null, "bad-token"])("rejects absent/invalid auth before RPC dispatch (%s)", async (token) => {
    expect((await rpc("tools/list", {}, token)).status).toBe(401);
  });

  it("counts each external RPC once, and cannot waive direct REST rate limits", async () => {
    const token = `rate-limit-test-${crypto.randomUUID()}`;
    const bindings = { ...env, RATE_LIMIT_PER_MINUTE: "1", AGENT_BUS_TOKENS: JSON.stringify({ [token]: { agent: `rate-agent-${crypto.randomUUID()}`, role: "operator" } }) };
    const request = () => new Request("https://worker.test/mcp", { method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" }, body: JSON.stringify({ jsonrpc: "2.0", id: 1, method: "tools/call", params: { name: "list_presence", arguments: {} } }) });
    expect((await app.request(request(), undefined, bindings)).status).toBe(200);
    expect((await app.request(request(), undefined, bindings)).status).toBe(429);
    expect((await app.request("/presence", { headers: { authorization: `Bearer ${token}` } }, bindings)).status).toBe(429);
  });

  it.each([
    { name: "post_message", arguments: { sender: "claude", recipient: "codex", topic: "status" } },
    { name: "list_messages", arguments: { limit: 0 } },
    { name: "list_messages", arguments: { limit: 1.5 } },
    { name: "list_messages", arguments: { tag: ["ok", 4] } },
    { name: "set_presence", arguments: { agent: "claude", ttl_seconds: 86401 } },
    { name: "bus_health", arguments: { origin_hub: "asuspro13" } },
    { name: "bus_health", arguments: [] },
    { name: "claim_resource", arguments: { agent: "claude", resource: "x" } },
    { name: "check_inbox", arguments: { agent: "claude" } },
  ])("rejects unsupported tool/invalid arguments before side effects: %j", async (params) => {
    expect((await rpc("tools/call", params)).body.error?.code).toBe(-32602);
  });

  it("retains PHI/body validation but never echoes rejection values", async () => {
    const response = await call("post_message", { sender: "claude", recipient: "codex", topic: "status", body: "x".repeat(262145) });
    rejected(response, 400);
    expect(JSON.stringify(response.body).length).toBeLessThan(300);
  });

  it("reports internal dispatch failures without disclosing implementation details", async () => {
    const request = new Request("https://worker.test/mcp", { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify({ jsonrpc: "2.0", id: "x", method: "tools/call", params: { name: "bus_health" } }) });
    const response = await handleMcp(request, async () => { throw new Error("private database detail"); });
    expect(await response.json()).toEqual({ jsonrpc: "2.0", id: "x", error: { code: -32603, message: "internal error" } });
  });

  it("rejects foreign/null Origins, unsupported methods and protocol headers", async () => {
    for (const origin of ["https://foreign.test", "null"]) {
      expect((await api("/mcp", { method: "POST", headers: { origin }, body: "{}" })).status).toBe(403);
    }
    expect((await api("/mcp")).status).toBe(405);
    expect((await rpc("resources/list")).body.error?.code).toBe(-32601);
    expect((await rpc("initialize", {})).body.error?.code).toBe(-32602);
    expect((await rpc("tools/list", { cursor: "not-issued" })).body.error?.code).toBe(-32602);
    expect((await api("/mcp", { method: "POST", headers: { "mcp-protocol-version": "future" }, body: "{}" })).status).toBe(400);
  });

  it("rejects malformed, batched, wrong-version, null-ID and mutating notifications", async () => {
    for (const body of ["{", "[]", '{"jsonrpc":"1.0","id":1,"method":"ping"}', '{"jsonrpc":"2.0","id":null,"method":"ping"}', '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":[]}']) {
      expect((await api("/mcp", { method: "POST", body })).status).toBe(400);
    }
    const response = await api("/mcp", { method: "POST", body: JSON.stringify({ jsonrpc: "2.0", method: "tools/call", params: { name: "knock_agent", arguments: { sender: "claude", recipient: "mutating-notification" } } }) });
    expect(response.status).toBe(400);
    expect((await apiJson<unknown[]>("/messages?agent=mutating-notification&broadcast=false")).body).toEqual([]);
  });

  it("bounds streamed bytes independently of Content-Length and rejects wrong media type", async () => {
    expect((await api("/mcp", { method: "POST", body: " ".repeat(1_048_577) })).status).toBe(413);
    expect((await api("/mcp", { method: "POST", headers: { "content-type": "text/plain" }, body: "{}" })).status).toBe(415);
  });
});
