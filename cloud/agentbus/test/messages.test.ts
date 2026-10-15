import { env } from "cloudflare:workers";
import { runInDurableObject } from "cloudflare:test";
import { describe, expect, it, vi } from "vitest";
import app from "../src/index";
import type { ReadMessagesQuery } from "../src/do-buslog";
import { apiJson, CLAUDE_TOKEN, CODEX_TOKEN, HUB_A, HUB_A_TOKEN, HUB_B, HUB_B_TOKEN, postJson } from "./helpers";

describe("rate admission exception diagnostics", () => {
  function failingBindings(failure: unknown) {
    const stub = { checkRateLimit: vi.fn(() => { throw failure; }) };
    return {
      ...env,
      BUS_LOG: { idFromName: () => "global", get: () => stub } as unknown as typeof env.BUS_LOG,
    };
  }

  it("records bounded exception fields without serializing request or error context", async () => {
    const privateMarker = "private-request-context-must-not-be-serialized";
    const failure = Object.assign(new Error("controlled storage failure ".repeat(40)), {
      headers: { authorization: privateMarker },
      body: privateMarker,
      config: { AGENT_BUS_TOKENS: privateMarker },
    });
    failure.name = `StorageFailure${"x".repeat(80)}`;
    failure.stack = "controlled storage stack\n".repeat(120);
    const logged = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const response = await app.request("/messages", {
        method: "POST",
        headers: { authorization: `Bearer ${CLAUDE_TOKEN}`, "x-private-context": privateMarker },
        body: privateMarker,
      }, failingBindings(failure));
      expect(response.status).toBe(500);
      expect(response.headers.get("content-type")).toBe("text/plain; charset=UTF-8");
      expect(await response.text()).toBe("Internal Server Error");
      const encoded = logged.mock.calls.find(([value]) => typeof value === "string")?.[0];
      expect(typeof encoded).toBe("string");
      const metadata = JSON.parse(encoded as string);
      expect(metadata).toEqual({
        event: "rate_admission_failure",
        name: failure.name.slice(0, 64),
        message: failure.message.slice(0, 512),
        stack: failure.stack.slice(0, 2048),
      });
      expect(encoded).not.toContain(privateMarker);
      // Hono must receive the SAME rejection, not a diagnostic replacement.
      expect(logged.mock.calls.find(([value]) => value === failure)?.[0]).toBe(failure);
    } finally {
      logged.mockRestore();
    }
  });

  it("rethrows the original error even if the additional diagnostic logger fails", async () => {
    const failure = new Error("original controlled storage rejection");
    const logged = vi.spyOn(console, "error")
      .mockImplementationOnce(() => { throw new Error("controlled diagnostic logger failure"); })
      .mockImplementation(() => {});
    try {
      const response = await app.request("/presence", {
        headers: { authorization: `Bearer ${CLAUDE_TOKEN}` },
      }, failingBindings(failure));
      expect(response.status).toBe(500);
      expect(await response.text()).toBe("Internal Server Error");
      expect(logged.mock.calls.at(-1)?.[0]).toBe(failure);
    } finally {
      logged.mockRestore();
    }
  });

  it("preserves a non-Error rejection without inspecting or serializing it", async () => {
    const failure = {
      body: "private non-Error context",
      toString: () => { throw new Error("unsafe string conversion"); },
      toJSON: () => { throw new Error("unsafe JSON conversion"); },
    };
    const logged = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      await expect(app.request("/presence", {
        headers: { authorization: `Bearer ${CLAUDE_TOKEN}` },
      }, failingBindings(failure))).rejects.toBe(failure);
      expect(JSON.parse(logged.mock.calls[0]?.[0] as string)).toEqual({
        event: "rate_admission_failure", name: "NonError", message: "non-Error rejection",
      });
    } finally {
      logged.mockRestore();
    }
  });

  it("keeps unauthenticated and rate-denied requests outside diagnostic logging", async () => {
    const stub = { checkRateLimit: vi.fn(() => false) };
    const bindings = {
      ...env,
      BUS_LOG: { idFromName: () => "global", get: () => stub } as unknown as typeof env.BUS_LOG,
    };
    const logged = vi.spyOn(console, "error").mockImplementation(() => {});
    try {
      const denied = await app.request("/presence", {}, bindings);
      expect(denied.status).toBe(401);
      expect(stub.checkRateLimit).not.toHaveBeenCalled();
      const throttled = await app.request("/presence", {
        headers: { authorization: `Bearer ${CLAUDE_TOKEN}` },
      }, bindings);
      expect(throttled.status).toBe(429);
      expect(throttled.headers.get("retry-after")).toBe("60");
      expect(await throttled.json()).toEqual({ error: "rate limit exceeded" });
      expect(stub.checkRateLimit).toHaveBeenCalledTimes(1);
      expect(logged).not.toHaveBeenCalled();
    } finally {
      logged.mockRestore();
    }
  });
});

describe("POST /messages", () => {
  it("sends a message and returns the stored Message shape", async () => {
    const { status, body } = await postJson<Record<string, unknown>>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "STATUS: hello from the cloud tier",
    });
    expect(status).toBe(200);
    expect(body).toMatchObject({
      from: "claude",
      to: "codex",
      topic: "status",
      body: "STATUS: hello from the cloud tier",
      protocol_version: "1.0",
      priority: "normal",
      request_ack: false,
      tags: [],
    });
    expect(typeof body.id).toBe("string");
    expect(typeof body.timestamp_utc).toBe("string");
    // Rust format: exactly 6 fractional-second digits.
    expect(body.timestamp_utc).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{6}Z$/);
    // skip_serializing_if fields must be OMITTED, not null, when absent.
    expect(body).not.toHaveProperty("thread_id");
    expect(body).not.toHaveProperty("reply_to");
  });

  it("an agent-role token defaults an empty/omitted sender to its own identity (re-review N9)", async () => {
    // auth.ts's docblock always said an omitted sender is filled from the
    // token; validateMessageCore used to contradict that by throwing on an
    // empty sender BEFORE bindAgent ever got a chance to default it.
    const { status, body } = await postJson<{ from: string }>("/messages", {
      sender: "",
      recipient: "codex",
      topic: "status",
      body: "hi",
    });
    expect(status).toBe(200);
    expect(body.from).toBe("claude");
  });

  it("a hub/operator token still rejects an empty sender (no identity to default to)", async () => {
    const { status, body } = await postJson<{ error: string }>(
      "/messages",
      { sender: "", recipient: "codex", topic: "status", body: "hi" },
      HUB_A_TOKEN,
    );
    expect(status).toBe(400);
    expect(body.error).toMatch(/sender must not be empty/);
  });

  it("rejects an invalid priority (400)", async () => {
    const { status, body } = await postJson<{ error: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "hi",
      priority: "critical",
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/invalid priority/);
  });

  it("auto-fits an unstructured body to the 'finding' schema for a *-findings topic", async () => {
    const { status, body } = await postJson<{ body: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "code-findings",
      body: "memory leak in allocator",
    });
    expect(status).toBe(200);
    expect(body.body).toContain("FINDING:");
    expect(body.body).toContain("SEVERITY:");
  });

  it("rejects sensitivity=no-offsite (the bus must never carry it off-site)", async () => {
    const { status, body } = await postJson<{ error: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "internal only",
      sensitivity: "no-offsite",
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/no-offsite/);
  });

  it("rejects a body matching an obvious PHI pattern", async () => {
    const { status, body } = await postJson<{ error: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "patient SSN is 123-45-6789",
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/PHI/);
  });

  it("preserves an explicit client_msg_id (agent-hub#79 idempotency key)", async () => {
    const clientMsgId = "018f4c2e-aaaa-7000-8000-000000000001";
    const { body } = await postJson<{ client_msg_id?: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "carries a client id",
      client_msg_id: clientMsgId,
    });
    expect(body.client_msg_id).toBe(clientMsgId);
  });

  it("IS IDEMPOTENT on client_msg_id: a retried send returns 200 with the ORIGINAL message, not an error", async () => {
    const clientMsgId = "018f4c2e-bbbb-7000-8000-000000000002";
    const payload = {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "outbox replay after a timeout",
      client_msg_id: clientMsgId,
    };
    const first = await postJson<{ id: string; client_msg_id?: string }>("/messages", payload);
    expect(first.status).toBe(200);

    const retry = await postJson<{ id: string; client_msg_id?: string }>("/messages", payload);
    expect(retry.status).toBe(200);
    expect(retry.body.id).toBe(first.body.id);
    expect(retry.body.client_msg_id).toBe(clientMsgId);
  });
});

describe("GET /messages", () => {
  it("round-trips a sent message and respects the agent/broadcast filter", async () => {
    await postJson("/messages", {
      sender: "claude",
      recipient: "read-test-agent",
      topic: "status",
      body: "STATUS: for read-test-agent",
    });
    await postJson("/messages", {
      sender: "claude",
      recipient: "someone-else",
      topic: "status",
      body: "STATUS: not for read-test-agent",
    });

    const { body } = await apiJson<Array<{ to: string; body: string }>>(
      "/messages?agent=read-test-agent&broadcast=false",
    );
    expect(body.length).toBeGreaterThanOrEqual(1);
    expect(body.every((m) => m.to === "read-test-agent")).toBe(true);
  });

  it("includes broadcast (to=all) messages only when broadcast=true", async () => {
    await postJson("/messages", {
      sender: "claude",
      recipient: "all",
      topic: "status",
      body: "STATUS: broadcast for everyone",
    });

    const withBroadcast = await apiJson<Array<{ to: string }>>(
      "/messages?agent=some-specific-agent&broadcast=true",
    );
    expect(withBroadcast.body.some((m) => m.to === "all")).toBe(true);

    const withoutBroadcast = await apiJson<Array<{ to: string }>>(
      "/messages?agent=some-specific-agent&broadcast=false",
    );
    expect(withoutBroadcast.body.some((m) => m.to === "all")).toBe(false);
  });

  it("filters by topic and tag", async () => {
    await postJson("/messages", {
      sender: "claude",
      recipient: "tag-test-agent",
      topic: "benchmark",
      body: "n=100",
      tags: ["repo:agent-hub", "priority:high"],
    });

    const { body } = await apiJson<Array<{ topic: string; tags: string[] }>>(
      "/messages?agent=tag-test-agent&topic=benchmark&tag=repo:agent-hub",
    );
    expect(body.length).toBeGreaterThanOrEqual(1);
    expect(body[0]?.topic).toBe("benchmark");
    expect(body[0]?.tags).toContain("repo:agent-hub");
  });
});

describe("POST /messages/batch", () => {
  it("posts up to 100 messages in one call", async () => {
    const { status, body } = await postJson<{ ids: string[]; count: number }>("/messages/batch", {
      messages: [
        { sender: "claude", recipient: "batch-agent", topic: "status", body: "one" },
        { sender: "claude", recipient: "batch-agent", topic: "status", body: "two" },
      ],
    });
    expect(status).toBe(200);
    expect(body.count).toBe(2);
    expect(body.ids).toHaveLength(2);
  });

  it("rejects an empty batch", async () => {
    const { status } = await postJson("/messages/batch", { messages: [] });
    expect(status).toBe(400);
  });

  it("names the failing item by index", async () => {
    const { status, body } = await postJson<{ error: string }>("/messages/batch", {
      messages: [
        { sender: "claude", recipient: "batch-agent", topic: "status", body: "ok" },
        { sender: "claude", recipient: "batch-agent", topic: "status", body: "bad", priority: "critical" },
      ],
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/^item 1:/);
  });

  it("is atomic: a later item's validation failure leaves NO earlier item persisted (re-review N8)", async () => {
    const { status } = await postJson("/messages/batch", {
      messages: [
        { sender: "claude", recipient: "batch-atomic-agent", topic: "status", body: "should-not-persist" },
        { sender: "claude", recipient: "batch-atomic-agent", topic: "status", body: "bad", priority: "critical" },
      ],
    });
    expect(status).toBe(400);

    const { body: stored } = await apiJson<Array<{ body: string }>>("/messages?agent=batch-atomic-agent");
    expect(stored.some((m) => m.body === "should-not-persist")).toBe(false);
  });

  it("an identity-binding mismatch inside a batch item is a 403, not a 400 (re-review N8)", async () => {
    const { status, body } = await postJson<{ error: string }>(
      "/messages/batch",
      {
        messages: [
          { sender: "claude", recipient: "batch-mismatch-agent", topic: "status", body: "should-not-persist-either" },
          { sender: "codex", recipient: "batch-mismatch-agent", topic: "status", body: "spoofed sender" },
        ],
      },
      CLAUDE_TOKEN,
    );
    expect(status).toBe(403);
    expect(body.error).toMatch(/^item 1:/);

    // Atomic: item 0 (valid on its own) must not have persisted either.
    const { body: stored } = await apiJson<Array<{ body: string }>>("/messages?agent=batch-mismatch-agent");
    expect(stored.some((m) => m.body === "should-not-persist-either")).toBe(false);
  });
});

describe("POST /messages/:id/ack", () => {
  it("acks a message and clears its pending-ack record", async () => {
    // Recipient is "claude" itself (a self-addressed message) so the acking
    // identity below IS the recipient (re-review N6: acking is now
    // restricted to the recipient, or a hub/operator token — see the
    // dedicated N6 describe block for the rejection case).
    const sent = await postJson<{ id: string }>("/messages", {
      sender: "claude",
      recipient: "claude",
      topic: "status",
      body: "needs an ack",
      request_ack: true,
    });
    const { status, body } = await postJson<{ ack_sent: boolean; acked_message_id: string }>(
      `/messages/${sent.body.id}/ack`,
      { agent: "claude" },
    );
    expect(status).toBe(200);
    expect(body.ack_sent).toBe(true);
    expect(body.acked_message_id).toBe(sent.body.id);

    const pending = await apiJson<Array<{ message_id: string }>>("/pending-acks?agent=claude");
    expect(pending.body.some((p) => p.message_id === sent.body.id)).toBe(false);
  });

  it("an agent-role token defaults an omitted/empty agent to its own identity", async () => {
    const { status, body } = await postJson<{ ack_sent: boolean }>("/messages/some-id/ack", { agent: "" });
    expect(status).toBe(200);
    expect(body.ack_sent).toBe(true);
  });

  it("a hub/operator token still rejects an empty agent (no identity to default to)", async () => {
    const { status } = await postJson("/messages/some-id/ack", { agent: "" }, HUB_A_TOKEN);
    expect(status).toBe(400);
  });

  it("REJECTS acking as a different agent (403 — review 'ack only for yourself')", async () => {
    // Recipient is "codex" so the eventual codex-authenticated ack below is
    // ALSO the recipient — isolating this test to the identity-binding
    // check (bindAgent) rather than tripping the separate N6 recipient
    // check too.
    const sent = await postJson<{ id: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "needs an ack",
      request_ack: true,
    });
    // Authenticated as claude (default token), but the body asserts a
    // different agent's identity.
    const { status } = await postJson(`/messages/${sent.body.id}/ack`, { agent: "codex" });
    expect(status).toBe(403);
    // A codex-authenticated ack for the same message is fine — codex IS the
    // recipient.
    const asCodex = await postJson<{ ack_sent: boolean }>(
      `/messages/${sent.body.id}/ack`,
      { agent: "codex" },
      CODEX_TOKEN,
    );
    expect(asCodex.status).toBe(200);
  });

  it("REJECTS acking a message addressed to someone else, even as your own valid identity (re-review N6)", async () => {
    const sent = await postJson<{ id: string }>("/messages", {
      sender: "claude",
      recipient: "someone-else",
      topic: "status",
      body: "needs an ack",
      request_ack: true,
    });
    // codex authenticates as itself (no identity-binding violation) but is
    // not the recipient ("someone-else") — this used to return 200 and
    // silently clear "someone-else"'s pending ack.
    const { status } = await postJson(`/messages/${sent.body.id}/ack`, { agent: "codex" }, CODEX_TOKEN);
    expect(status).toBe(403);

    const pending = await apiJson<Array<{ message_id: string }>>("/pending-acks?agent=someone-else");
    expect(pending.body.some((p) => p.message_id === sent.body.id)).toBe(true);
  });

  it("a broadcast (to: 'all') may be acked by any agent (re-review N6)", async () => {
    const sent = await postJson<{ id: string }>("/messages", {
      sender: "claude",
      recipient: "all",
      topic: "status",
      body: "broadcast needing ack",
      request_ack: true,
    });
    const { status } = await postJson(`/messages/${sent.body.id}/ack`, { agent: "codex" }, CODEX_TOKEN);
    expect(status).toBe(200);
  });

  it("a hub-role token may ack a message addressed to a different agent (vouching, re-review N6)", async () => {
    const sent = await postJson<{ id: string }>("/messages", {
      sender: "claude",
      recipient: "someone-else",
      topic: "status",
      body: "needs an ack",
      request_ack: true,
    });
    const { status } = await postJson(
      `/messages/${sent.body.id}/ack`,
      { agent: "someone-else" },
      HUB_A_TOKEN,
    );
    expect(status).toBe(200);
  });
});

describe("POST /read/batch and POST /ack/batch", () => {
  it("returns complete multi-agent pages above the RPC value-size limit", async () => {
    const agents = [crypto.randomUUID(), crypto.randomUUID()];
    const payload = "x".repeat(225 * 1024);
    const ids: string[] = [];
    const now = Date.now();
    await runInDurableObject(env.BUS_LOG.get(env.BUS_LOG.idFromName("global")), async (instance) => {
      for (let index = 0; index < 160; index++) {
        const id = crypto.randomUUID();
        ids.push(id);
        instance.insertMessage({
          id, origin_hub: "large-page-origin",
          timestamp_utc: new Date(now + index).toISOString(),
          protocol_version: "1.0", from: "claude", to: agents[index % 2]!,
          topic: "status", body: payload, tags: [], priority: "normal",
          request_ack: false, metadata: {},
        });
      }
    });
    const response = await app.request("/read/batch", {
      method: "POST",
      headers: { authorization: `Bearer ${CLAUDE_TOKEN}`, "content-type": "application/json" },
      body: JSON.stringify({ agents, limit: 80, broadcast: false }),
    }, env);
    expect(response.status).toBe(200);
    expect(response.headers.get("content-type")).toContain("application/json");
    expect(response.headers.get("cache-control")).toBe("no-store");
    const messages = await response.json() as Array<{ id: string; body: string; to: string }>;
    expect(messages.map((message) => message.id)).toEqual(ids);
    expect(messages.filter((message) => message.to === agents[0])).toHaveLength(80);
    expect(messages.filter((message) => message.to === agents[1])).toHaveLength(80);
    expect(messages.every((message) => message.body.length === payload.length)).toBe(true);
  }, 30_000);

  it("uses one rate-limit RPC and one read RPC for a multi-agent batch", async () => {
    const stub = {
      checkRateLimit: vi.fn(() => true),
      listMessagesBatch: vi.fn((_queries: ReadMessagesQuery[]) => new Response("[]", {
        headers: { "content-type": "application/json" },
      })),
      listMessages: vi.fn(() => []),
    };
    const bindings = {
      ...env,
      BUS_LOG: { idFromName: () => "global", get: () => stub } as unknown as typeof env.BUS_LOG,
    };
    const response = await app.request("/read/batch", {
      method: "POST",
      headers: { authorization: `Bearer ${CLAUDE_TOKEN}`, "content-type": "application/json" },
      body: JSON.stringify({ agents: ["batch-rpc-a", "batch-rpc-b"], limit: 3, broadcast: false }),
    }, bindings);
    expect(response.status).toBe(200);
    expect(stub.checkRateLimit).toHaveBeenCalledTimes(1);
    expect(stub.listMessagesBatch).toHaveBeenCalledTimes(1);
    expect(stub.listMessages).not.toHaveBeenCalled();
    expect(stub.listMessagesBatch.mock.calls[0]?.[0]).toMatchObject([
      { agent: "batch-rpc-a", limit: 3, include_broadcast: false },
      { agent: "batch-rpc-b", limit: 3, include_broadcast: false },
    ]);
  });

  it("retains distinct origins sharing an id and deduplicates each broadcast once", async () => {
    const sharedId = crypto.randomUUID();
    const agent = `batch-origin-${crypto.randomUUID()}`;
    for (const [origin, token] of [[HUB_A, HUB_A_TOKEN], [HUB_B, HUB_B_TOKEN]]) {
      const sent = await postJson("/sync/push", {
        origin_hub: origin,
        messages: [{ id: sharedId, sender: "claude", recipient: "all", topic: "status", body: `from ${origin}` }],
      }, token);
      expect(sent.status).toBe(200);
    }

    const read = await postJson<Array<{ id: string; origin_hub: string }>>("/read/batch", {
      agents: [agent, `${agent}-other`, agent],
      broadcast: true,
    });
    expect(read.status).toBe(200);
    expect(read.body.filter((message) => message.id === sharedId).map((message) => message.origin_hub).sort())
      .toEqual([HUB_A, HUB_B].sort());
  });

  it("preserves each agent's newest-message limit and broadcast filter", async () => {
    const agents = [crypto.randomUUID(), crypto.randomUUID()];
    const latestIds = [];
    for (const recipient of agents) {
      await postJson("/messages", { sender: "claude", recipient, topic: "status", body: "older" });
      const latest = await postJson<{ id: string }>("/messages", {
        sender: "claude", recipient, topic: "status", body: "latest",
      });
      expect(latest.status).toBe(200);
      latestIds.push(latest.body.id);
    }
    await postJson("/messages", { sender: "claude", recipient: "all", topic: "status", body: "broadcast" });

    const read = await postJson<Array<{ id: string; timestamp_utc: string }>>("/read/batch", {
      agents, limit: 1, broadcast: false,
    });
    expect(read.status).toBe(200);
    expect(read.body.map((message) => message.id)).toEqual(latestIds);
    expect(read.body.map((message) => message.timestamp_utc)).toEqual(
      read.body.map((message) => message.timestamp_utc).sort(),
    );
  });

  it("reads across multiple agents, deduping broadcast messages", async () => {
    await postJson("/messages", {
      sender: "claude",
      recipient: "batch-read-a",
      topic: "status",
      body: "for a",
    });
    await postJson("/messages", {
      sender: "claude",
      recipient: "batch-read-b",
      topic: "status",
      body: "for b",
    });

    const { status, body } = await postJson<Array<{ to: string }>>("/read/batch", {
      agents: ["batch-read-a", "batch-read-b"],
    });
    expect(status).toBe(200);
    expect(body.some((m) => m.to === "batch-read-a")).toBe(true);
    expect(body.some((m) => m.to === "batch-read-b")).toBe(true);
  });

  it("batch-acks several message ids at once", async () => {
    // Both messages are addressed to "claude" itself, so the acking
    // identity below (also claude) is the recipient (re-review N6).
    const first = await postJson<{ id: string }>(
      "/messages",
      { sender: "codex", recipient: "claude", topic: "status", body: "one", request_ack: true },
      CODEX_TOKEN,
    );
    const second = await postJson<{ id: string }>(
      "/messages",
      { sender: "codex", recipient: "claude", topic: "status", body: "two", request_ack: true },
      CODEX_TOKEN,
    );

    const { status, body } = await postJson<{ acked: number; message_ids: string[] }>("/ack/batch", {
      agent: "claude",
      message_ids: [first.body.id, second.body.id],
    });
    expect(status).toBe(200);
    expect(body.acked).toBe(2);
    expect(body.message_ids).toEqual([first.body.id, second.body.id]);
  });

  it("REJECTS a batch ack when ANY id in the batch is addressed to someone else, applying none of them (re-review N6)", async () => {
    const mine = await postJson<{ id: string }>(
      "/messages",
      { sender: "codex", recipient: "claude", topic: "status", body: "mine", request_ack: true },
      CODEX_TOKEN,
    );
    const notMine = await postJson<{ id: string }>(
      "/messages",
      { sender: "codex", recipient: "someone-else", topic: "status", body: "not mine", request_ack: true },
      CODEX_TOKEN,
    );

    const { status } = await postJson("/ack/batch", {
      agent: "claude",
      message_ids: [mine.body.id, notMine.body.id],
    });
    expect(status).toBe(403);

    // Neither pending ack was cleared — the unauthorized id in the batch
    // blocked the whole batch, not just itself.
    const pendingMine = await apiJson<Array<{ message_id: string }>>("/pending-acks?agent=claude");
    expect(pendingMine.body.some((p) => p.message_id === mine.body.id)).toBe(true);
    const pendingNotMine = await apiJson<Array<{ message_id: string }>>("/pending-acks?agent=someone-else");
    expect(pendingNotMine.body.some((p) => p.message_id === notMine.body.id)).toBe(true);
  });
});

describe("POST /knock", () => {
  it("posts an urgent knock message with knock metadata", async () => {
    const { status, body } = await postJson<{ topic: string; priority: string; metadata: Record<string, unknown> }>(
      "/knock",
      { sender: "claude", recipient: "codex" },
    );
    expect(status).toBe(200);
    expect(body.topic).toBe("knock");
    expect(body.priority).toBe("urgent");
    expect(body.metadata).toMatchObject({ knock: true, delivery_hint: "sse" });
  });
});
