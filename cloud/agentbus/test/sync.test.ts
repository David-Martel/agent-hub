import { env } from "cloudflare:workers";
import { runInDurableObject } from "cloudflare:test";
import { describe, expect, it, vi } from "vitest";
import { BusLog } from "../src/do-buslog";
import { apiJson, CLAUDE_TOKEN, HUB_A, HUB_A_TOKEN, HUB_B, HUB_B_TOKEN, OPERATOR_TOKEN, postJson } from "./helpers";

// agent-hub#82 security review: /sync/* now requires a hub-role token
// (H1/H5), and origin_hub for the whole push comes from that token's own
// `hub` identity, never the request body. HUB_A_TOKEN speaks for "asuspro13"
// (== HUB_A) and HUB_B_TOKEN for "spark-0060" (== HUB_B) — see
// test/helpers.ts / vitest.config.ts.

describe("POST /sync/push auth (review H1/H5)", () => {
  it("REJECTS a non-hub token (403) — /sync/* used to be open to any authenticated caller", async () => {
    const { status } = await postJson("/sync/push", {
      origin_hub: HUB_A,
      messages: [{ id: crypto.randomUUID(), sender: "claude", recipient: "codex", topic: "status", body: "x" }],
    }); // default CLAUDE_TOKEN, role "agent"
    expect(status).toBe(403);
  });

  it("REJECTS a body origin_hub that disagrees with the token's own hub (403)", async () => {
    const { status } = await postJson(
      "/sync/push",
      {
        origin_hub: HUB_B, // HUB_A_TOKEN actually speaks for HUB_A
        messages: [],
      },
      HUB_A_TOKEN,
    );
    expect(status).toBe(403);
  });
});

describe("POST /sync/push", () => {
  it("accepts a batch and returns a cursor", async () => {
    const { status, body } = await postJson<{ accepted: string[]; duplicates: string[]; cursor: number }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [
          {
            id: crypto.randomUUID(),
            sender: "claude",
            recipient: "codex",
            topic: "status",
            body: "STATUS: synced from asuspro13",
            client_msg_id: crypto.randomUUID(),
          },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(status).toBe(200);
    expect(body.accepted).toHaveLength(1);
    expect(body.duplicates).toHaveLength(0);
    expect(typeof body.cursor).toBe("number");
  });

  it("is IDEMPOTENT on re-push: the same id is reported as a duplicate, not re-inserted", async () => {
    const id = crypto.randomUUID();
    const message = {
      id,
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "STATUS: idempotency check",
    };
    const first = await postJson<{ accepted: string[]; duplicates: string[] }>(
      "/sync/push",
      { origin_hub: HUB_A, messages: [message] },
      HUB_A_TOKEN,
    );
    expect(first.body.accepted).toEqual([id]);

    const second = await postJson<{ accepted: string[]; duplicates: string[] }>(
      "/sync/push",
      { origin_hub: HUB_A, messages: [message] },
      HUB_A_TOKEN,
    );
    expect(second.body.accepted).toHaveLength(0);
    expect(second.body.duplicates).toEqual([id]);
  });

  it("is idempotent on client_msg_id even when the server-side id differs", async () => {
    const clientMsgId = crypto.randomUUID();
    const push = (id: string) =>
      postJson<{ accepted: string[]; duplicates: string[] }>(
        "/sync/push",
        {
          origin_hub: HUB_A,
          messages: [
            {
              id,
              client_msg_id: clientMsgId,
              sender: "claude",
              recipient: "codex",
              topic: "status",
              body: "STATUS: outbox replay",
            },
          ],
        },
        HUB_A_TOKEN,
      );
    const first = await push(crypto.randomUUID());
    expect(first.body.accepted).toHaveLength(1);
    const second = await push(crypto.randomUUID()); // different server id, same client_msg_id
    expect(second.body.accepted).toHaveLength(0);
    expect(second.body.duplicates).toHaveLength(1);
  });

  it("reports a CONFLICT (not a silent duplicate) when the SAME id from the SAME origin carries DIFFERENT content (review M5)", async () => {
    const id = crypto.randomUUID();
    const first = await postJson<{ accepted: string[] }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [{ id, sender: "claude", recipient: "codex", topic: "status", body: "original content" }],
      },
      HUB_A_TOKEN,
    );
    expect(first.body.accepted).toEqual([id]);

    const second = await postJson<{ accepted: string[]; duplicates: string[]; conflicts: string[] }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [{ id, sender: "claude", recipient: "codex", topic: "status", body: "DIFFERENT content" }],
      },
      HUB_A_TOKEN,
    );
    expect(second.body.accepted).toHaveLength(0);
    expect(second.body.duplicates).toHaveLength(0);
    expect(second.body.conflicts).toEqual([id]);
  });

  it("a cross-origin id collision does NOT pre-empt the other origin's message (review M5/P5)", async () => {
    const id = crypto.randomUUID();
    const fromA = await postJson<{ accepted: string[] }>(
      "/sync/push",
      { origin_hub: HUB_A, messages: [{ id, sender: "claude", recipient: "codex", topic: "status", body: "from hub A" }] },
      HUB_A_TOKEN,
    );
    expect(fromA.body.accepted).toEqual([id]);

    // A DIFFERENT hub pushing the SAME id is a genuinely new row (scoped by
    // origin_hub), not a duplicate or a pre-emption of hub A's message.
    const fromB = await postJson<{ accepted: string[] }>(
      "/sync/push",
      { origin_hub: HUB_B, messages: [{ id, sender: "claude", recipient: "codex", topic: "status", body: "from hub B" }] },
      HUB_B_TOKEN,
    );
    expect(fromB.body.accepted).toEqual([id]);

    const pulled = await apiJson<{ messages: Array<{ id: string; body: string; origin_hub?: string }> }>(
      `/sync/pull?since=0&limit=100000`,
      undefined,
      OPERATOR_TOKEN,
    );
    const rows = pulled.body.messages.filter((m) => m.id === id);
    expect(rows).toHaveLength(2);
    expect(rows.some((r) => r.body === "from hub A" && r.origin_hub === HUB_A)).toBe(true);
    expect(rows.some((r) => r.body === "from hub B" && r.origin_hub === HUB_B)).toBe(true);
  });

  it("REJECTS sensitivity=no-offsite messages (never replicated to the cloud tier)", async () => {
    const { status, body } = await postJson<{
      accepted: string[];
      rejected: Array<{ reason: string }>;
    }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [
          {
            id: crypto.randomUUID(),
            sender: "claude",
            recipient: "codex",
            topic: "status",
            body: "internal only",
            sensitivity: "no-offsite",
          },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(status).toBe(200);
    expect(body.accepted).toHaveLength(0);
    expect(body.rejected).toHaveLength(1);
    expect(body.rejected[0]?.reason).toMatch(/no-offsite/);
  });

  it("fails closed on an unrecognized sensitivity value (review H4 — was accepted verbatim and stored)", async () => {
    const { body } = await postJson<{ accepted: string[]; rejected: Array<{ reason: string }> }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [
          {
            id: crypto.randomUUID(),
            sender: "claude",
            recipient: "codex",
            topic: "status",
            body: "x",
            sensitivity: "No-Offsite", // wrong case — used to be accepted verbatim
          },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(body.accepted).toHaveLength(0);
    expect(body.rejected).toHaveLength(1);
  });

  it("rejects an oversized body per-item without failing the whole batch (review H3)", async () => {
    const good = crypto.randomUUID();
    const bad = crypto.randomUUID();
    const { body } = await postJson<{ accepted: string[]; rejected: Array<{ id?: string; reason: string }> }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [
          { id: good, sender: "claude", recipient: "codex", topic: "status", body: "fine" },
          { id: bad, sender: "claude", recipient: "codex", topic: "status", body: "x".repeat(300_000) },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(body.accepted).toEqual([good]);
    expect(body.rejected.some((r) => r.id === bad)).toBe(true);
  });

  it("rejects a non-array tags value per-item (review H7 — used to corrupt every future tag-scoped read)", async () => {
    const { body } = await postJson<{ accepted: string[]; rejected: Array<{ reason: string }> }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [
          { id: crypto.randomUUID(), sender: "claude", recipient: "codex", topic: "status", body: "x", tags: 5 },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(body.accepted).toHaveLength(0);
    expect(body.rejected[0]?.reason).toMatch(/tags/);
  });

  it("rejects a body matching an obvious PHI pattern (review H3 — PHI screen was skipped entirely on sync push)", async () => {
    const { body } = await postJson<{ accepted: string[]; rejected: Array<{ reason: string }> }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [
          { id: crypto.randomUUID(), sender: "claude", recipient: "codex", topic: "status", body: "SSN is 123-45-6789" },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(body.accepted).toHaveLength(0);
    expect(body.rejected[0]?.reason).toMatch(/PHI/);
  });

  it("carries origin_seq through to storage (review H6 — used to be silently dropped)", async () => {
    const id = crypto.randomUUID();
    await postJson<{ accepted: string[] }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [
          { id, sender: "claude", recipient: "codex", topic: "status", body: "has an origin_seq", origin_seq: 42 },
        ],
      },
      HUB_A_TOKEN,
    );
    const pulled = await apiJson<{ messages: Array<{ id: string; origin_seq?: number }> }>(
      `/sync/pull?since=0&limit=100000`,
      undefined,
      OPERATOR_TOKEN,
    );
    const row = pulled.body.messages.find((m) => m.id === id);
    expect(row?.origin_seq).toBe(42);
  });

  it("a per-item origin_hub that disagrees with the request's origin_hub is rejected, not trusted (review H5)", async () => {
    const id = crypto.randomUUID();
    const { body } = await postJson<{ accepted: string[]; rejected: Array<{ id?: string; reason: string }> }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [
          { id, sender: "claude", recipient: "codex", topic: "status", body: "x", origin_hub: HUB_B },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(body.accepted).toHaveLength(0);
    expect(body.rejected.some((r) => r.id === id)).toBe(true);
  });

  it("does NOT rewrite the body via schema auto-fit (deliberate deviation — see SYNC-CONTRACT.md)", async () => {
    const id = crypto.randomUUID();
    await postJson<{ accepted: string[] }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [
          { id, sender: "claude", recipient: "codex", topic: "code-findings", body: "unstructured historical text" },
        ],
      },
      HUB_A_TOKEN,
    );
    const pulled = await apiJson<{ messages: Array<{ id: string; body: string }> }>(
      `/sync/pull?since=0&limit=100000`,
      undefined,
      OPERATOR_TOKEN,
    );
    const row = pulled.body.messages.find((m) => m.id === id);
    // Would contain "FINDING:"/"SEVERITY:" if schema auto-fit ran, the way
    // it does for a direct POST /messages to the same topic.
    expect(row?.body).toBe("unstructured historical text");
  });

  it("preserves a historical id/timestamp_utc/protocol_version but validates their format", async () => {
    const id = crypto.randomUUID();
    const ok = await postJson<{ accepted: string[] }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [
          {
            id,
            timestamp_utc: "2020-01-01T00:00:00.000000Z",
            protocol_version: "1.0",
            sender: "claude",
            recipient: "codex",
            topic: "status",
            body: "old but valid",
          },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(ok.body.accepted).toEqual([id]);

    const badTimestamp = await postJson<{ accepted: string[]; rejected: Array<{ reason: string }> }>(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: [
          {
            id: crypto.randomUUID(),
            timestamp_utc: "not-a-timestamp",
            sender: "claude",
            recipient: "codex",
            topic: "status",
            body: "x",
          },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(badTimestamp.body.accepted).toHaveLength(0);
  });

  it("origin_hub in the body is optional — it always comes from the hub token's own identity", async () => {
    const id = crypto.randomUUID();
    const { status, body } = await postJson<{ accepted: string[] }>(
      "/sync/push",
      { messages: [{ id, sender: "claude", recipient: "codex", topic: "status", body: "no origin_hub in body" }] },
      HUB_A_TOKEN,
    );
    expect(status).toBe(200);
    expect(body.accepted).toEqual([id]);
  });
});

describe("GET /sync/pull", () => {
  it("REJECTS a non-hub/operator token (403, review M3 — was a full-archive export for ANY token)", async () => {
    const { status } = await apiJson("/sync/pull?since=0", undefined, CLAUDE_TOKEN);
    expect(status).toBe(403);
  });

  it("a non-integer since is a 400, not a silently-empty page (review L2/P8)", async () => {
    const { status, body } = await apiJson<{ messages?: unknown[] }>("/sync/pull?since=abc", undefined, HUB_A_TOKEN);
    expect(status).toBe(400);
    expect(body.messages).toBeUndefined();
  });

  it("pages through pushed messages in order, advancing the cursor", async () => {
    // Earlier tests in this file already pushed messages into the same
    // (per-test-file-isolated) BusLog singleton, so `since=0` would return
    // THEIR messages first. Capture the cursor immediately before pushing
    // this test's own messages so pagination is scoped to just those.
    const baseline = await apiJson<{ next_cursor: number }>(`/sync/pull?since=0&limit=100000`, undefined, HUB_A_TOKEN);
    const startCursor = baseline.body.next_cursor;

    const ids = [crypto.randomUUID(), crypto.randomUUID(), crypto.randomUUID()];
    await postJson(
      "/sync/push",
      {
        origin_hub: HUB_A,
        messages: ids.map((id, i) => ({
          id,
          sender: "claude",
          recipient: "codex",
          topic: "status",
          body: `page-item-${i}`,
        })),
      },
      HUB_A_TOKEN,
    );

    const page1 = await apiJson<{ messages: Array<{ id: string }>; next_cursor: number; has_more: boolean }>(
      `/sync/pull?since=${startCursor}&limit=2`,
      undefined,
      HUB_A_TOKEN,
    );
    expect(page1.body.messages.length).toBeLessThanOrEqual(2);

    const page2 = await apiJson<{ messages: Array<{ id: string }>; next_cursor: number; has_more: boolean }>(
      `/sync/pull?since=${page1.body.next_cursor}&limit=2`,
      undefined,
      HUB_A_TOKEN,
    );
    const seenIds = new Set([...page1.body.messages, ...page2.body.messages].map((m) => m.id));
    // Every id we pushed for this hub must show up somewhere across the two pages.
    for (const id of ids) {
      expect(seenIds.has(id)).toBe(true);
    }
  });

  it("exclude_origin omits messages that originated at that hub", async () => {
    const id = crypto.randomUUID();
    const pushResult = await postJson<{ cursor: number }>(
      "/sync/push",
      { origin_hub: HUB_B, messages: [{ id, sender: "claude", recipient: "codex", topic: "status", body: "from spark-0060" }] },
      HUB_B_TOKEN,
    );

    const excluded = await apiJson<{ messages: Array<{ id: string }> }>(
      `/sync/pull?since=0&limit=500&exclude_origin=${HUB_B}`,
      undefined,
      OPERATOR_TOKEN,
    );
    expect(excluded.body.messages.some((m) => m.id === id)).toBe(false);

    const included = await apiJson<{ messages: Array<{ id: string }> }>(
      `/sync/pull?since=0&limit=500&exclude_origin=some-other-hub`,
      undefined,
      OPERATOR_TOKEN,
    );
    expect(included.body.messages.some((m) => m.id === id)).toBe(true);
    void pushResult;
  });
});

describe("POST /sync/push-presence (agent-hub#82 task item 6)", () => {
  it("accepts a batch and returns integer accepted/duplicates counts (matches the importer's expected shape)", async () => {
    const originId = Math.floor(Math.random() * 1_000_000_000);
    const { status, body } = await postJson<{ accepted: number; duplicates: number; rejected: unknown[] }>(
      "/sync/push-presence",
      {
        origin_hub: HUB_A,
        origin_host: "asuspro13",
        events: [
          {
            origin_id: originId,
            timestamp_utc: "2026-06-12T16:06:02.895291Z",
            protocol_version: "1.0",
            agent: "claude",
            status: "online",
            session_id: "s1",
            capabilities: ["mcp"],
            metadata: {},
            ttl_seconds: 180,
          },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(status).toBe(200);
    expect(body.accepted).toBe(1);
    expect(body.duplicates).toBe(0);
    expect(body.rejected).toEqual([]);
  });

  it("dedups on (origin_hub, origin_id)", async () => {
    const originId = Math.floor(Math.random() * 1_000_000_000);
    const event = {
      origin_id: originId,
      timestamp_utc: "2026-06-12T16:06:02.895291Z",
      protocol_version: "1.0",
      agent: "claude",
      status: "online",
    };
    const first = await postJson<{ accepted: number }>(
      "/sync/push-presence",
      { origin_hub: HUB_A, origin_host: "asuspro13", events: [event] },
      HUB_A_TOKEN,
    );
    expect(first.body.accepted).toBe(1);
    const second = await postJson<{ accepted: number; duplicates: number }>(
      "/sync/push-presence",
      { origin_hub: HUB_A, origin_host: "asuspro13", events: [event] },
      HUB_A_TOKEN,
    );
    expect(second.body.accepted).toBe(0);
    expect(second.body.duplicates).toBe(1);
  });

  it("REJECTS a non-hub token (403)", async () => {
    const { status } = await postJson(
      "/sync/push-presence",
      { origin_hub: HUB_A, origin_host: "asuspro13", events: [] },
      CLAUDE_TOKEN,
    );
    expect(status).toBe(403);
  });

  it("rejects presence metadata matching an obvious PHI pattern per event", async () => {
    const { body } = await postJson<{ accepted: number; rejected: Array<{ reason: string }> }>(
      "/sync/push-presence",
      {
        origin_hub: HUB_A,
        origin_host: "asuspro13",
        events: [
          {
            origin_id: Math.floor(Math.random() * 1_000_000_000),
            timestamp_utc: "2026-06-12T16:06:02.895291Z",
            protocol_version: "1.0",
            agent: "claude",
            status: "online",
            metadata: { note: "patient name is on file" },
          },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(body.accepted).toBe(0);
    expect(body.rejected[0]?.reason).toMatch(/PHI/);
  });
});

describe("GET /sync/stats (operator role)", () => {
  it("returns per-origin_hub counts", async () => {
    await postJson(
      "/sync/push",
      { origin_hub: HUB_A, messages: [{ id: crypto.randomUUID(), sender: "claude", recipient: "codex", topic: "status", body: "for stats" }] },
      HUB_A_TOKEN,
    );
    const { status, body } = await apiJson<{
      messages: Array<{ origin_hub: string; count: number }>;
      presence: Array<{ origin_hub: string; count: number }>;
    }>("/sync/stats", undefined, OPERATOR_TOKEN);
    expect(status).toBe(200);
    expect(body.messages.some((row) => row.origin_hub === HUB_A && row.count > 0)).toBe(true);
  });

  it("REJECTS a hub-role token (operator only)", async () => {
    const { status } = await apiJson("/sync/stats", undefined, HUB_A_TOKEN);
    expect(status).toBe(403);
  });
});

describe("syncPull snapshot cursor regression", () => {
  async function withBus(test: (bus: BusLog) => void): Promise<void> {
    const stub = env.BUS_LOG.get(env.BUS_LOG.idFromName(`cursor-regression-${crypto.randomUUID()}`));
    await runInDurableObject(stub, async (instance) => test(instance));
  }

  function insert(bus: BusLog, origin: string): string {
    const id = crypto.randomUUID();
    expect(bus.insertMessage({
      id,
      origin_hub: origin,
      timestamp_utc: new Date().toISOString(),
      protocol_version: "1.0",
      from: "cursor-sender",
      to: "cursor-recipient",
      topic: "status",
      body: "isolated cursor regression",
      tags: [],
      priority: "normal",
      request_ack: false,
      metadata: {},
    }).inserted).toBe(true);
    return id;
  }

  it("advances over an excluded-only tail and stays at the tail on an idle poll", async () => {
    await withBus((bus) => {
      insert(bus, HUB_A);
      insert(bus, HUB_A);
      insert(bus, HUB_A);
      const tail = bus.currentCursor();
      const first = bus.syncPull(0, HUB_A, 2);
      expect(first).toEqual({ messages: [], next_cursor: tail, has_more: false });
      expect(bus.syncPull(first.next_cursor, HUB_A, 2)).toEqual(first);
    });
  });

  it("never skips lookahead matches and advances through the final excluded suffix", async () => {
    await withBus((bus) => {
      insert(bus, HUB_A);
      const firstId = insert(bus, HUB_B);
      insert(bus, HUB_A);
      const secondId = insert(bus, HUB_B);
      const pageBoundary = bus.currentCursor();
      const thirdId = insert(bus, HUB_B);
      insert(bus, HUB_A);
      const tail = bus.currentCursor();
      const first = bus.syncPull(0, HUB_A, 2);
      expect(first.messages.map((message) => message.id)).toEqual([firstId, secondId]);
      expect(first.next_cursor).toBe(pageBoundary);
      expect(first.has_more).toBe(true);
      const second = bus.syncPull(first.next_cursor, HUB_A, 2);
      expect(second.messages.map((message) => message.id)).toEqual([thirdId]);
      expect(second.next_cursor).toBe(tail);
      expect(second.has_more).toBe(false);
    });
  });

  it.each([HUB_A, undefined])("delivers an append after the captured snapshot on the next poll (exclude=%s)", async (exclude) => {
    await withBus((bus) => {
      const firstId = insert(bus, HUB_A);
      const highWater = bus.currentCursor();
      let appendedId = "";
      // Model an append immediately after snapshot capture, using real SQLite
      // and the production query. Actual synchronous DO calls cannot interleave.
      const snapshot = vi.spyOn(bus, "currentCursor").mockImplementationOnce(() => {
        appendedId = insert(bus, HUB_B);
        return highWater;
      });
      let first: ReturnType<BusLog["syncPull"]>;
      try {
        first = bus.syncPull(0, exclude, 2);
      } finally {
        snapshot.mockRestore();
      }
      expect(first.messages.map((message) => message.id)).toEqual(exclude ? [] : [firstId]);
      expect(first.next_cursor).toBe(highWater);
      expect(first.has_more).toBe(false);
      const second = bus.syncPull(first.next_cursor, exclude, 2);
      expect(second.messages.map((message) => message.id)).toEqual([appendedId]);
      expect(second.next_cursor).toBe(bus.currentCursor());
      expect(second.has_more).toBe(false);
    });
  });

  it("preserves a future cursor in populated and empty storage", async () => {
    await withBus((bus) => {
      expect(bus.syncPull(999, HUB_A, 2)).toEqual({ messages: [], next_cursor: 999, has_more: false });
      insert(bus, HUB_B);
      expect(bus.syncPull(999, HUB_A, 2)).toEqual({ messages: [], next_cursor: 999, has_more: false });
    });
  });
});
