import { describe, expect, it } from "vitest";
import { apiJson, CODEX_TOKEN, HUB_A_TOKEN, postJson } from "./helpers";

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
