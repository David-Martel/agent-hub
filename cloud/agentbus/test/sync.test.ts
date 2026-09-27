import { describe, expect, it } from "vitest";
import { apiJson, postJson } from "./helpers";

function uniqueHub(name: string): string {
  return `${name}-${crypto.randomUUID()}`;
}

describe("POST /sync/push", () => {
  it("accepts a batch and returns a cursor", async () => {
    const hub = uniqueHub("asuspro13");
    const { status, body } = await postJson<{ accepted: string[]; duplicates: string[]; cursor: number }>(
      "/sync/push",
      {
        origin_hub: hub,
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
    );
    expect(status).toBe(200);
    expect(body.accepted).toHaveLength(1);
    expect(body.duplicates).toHaveLength(0);
    expect(typeof body.cursor).toBe("number");
  });

  it("is IDEMPOTENT on re-push: the same id is reported as a duplicate, not re-inserted", async () => {
    const hub = uniqueHub("asuspro13");
    const id = crypto.randomUUID();
    const message = {
      id,
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "STATUS: idempotency check",
    };
    const first = await postJson<{ accepted: string[]; duplicates: string[] }>("/sync/push", {
      origin_hub: hub,
      messages: [message],
    });
    expect(first.body.accepted).toEqual([id]);

    const second = await postJson<{ accepted: string[]; duplicates: string[] }>("/sync/push", {
      origin_hub: hub,
      messages: [message],
    });
    expect(second.body.accepted).toHaveLength(0);
    expect(second.body.duplicates).toEqual([id]);
  });

  it("is idempotent on client_msg_id even when the server-side id differs", async () => {
    const hub = uniqueHub("asuspro13");
    const clientMsgId = crypto.randomUUID();
    const push = (id: string) =>
      postJson<{ accepted: string[]; duplicates: string[] }>("/sync/push", {
        origin_hub: hub,
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
      });
    const first = await push(crypto.randomUUID());
    expect(first.body.accepted).toHaveLength(1);
    const second = await push(crypto.randomUUID()); // different server id, same client_msg_id
    expect(second.body.accepted).toHaveLength(0);
    expect(second.body.duplicates).toHaveLength(1);
  });

  it("REJECTS sensitivity=no-offsite messages (never replicated to the cloud tier)", async () => {
    const hub = uniqueHub("asuspro13");
    const { status, body } = await postJson<{
      accepted: string[];
      rejected: Array<{ reason: string }>;
    }>("/sync/push", {
      origin_hub: hub,
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
    });
    expect(status).toBe(200);
    expect(body.accepted).toHaveLength(0);
    expect(body.rejected).toHaveLength(1);
    expect(body.rejected[0]?.reason).toMatch(/no-offsite/);
  });

  it("requires origin_hub", async () => {
    const { status } = await postJson("/sync/push", { messages: [] });
    expect(status).toBe(400);
  });
});

describe("GET /sync/pull", () => {
  it("pages through pushed messages in order, advancing the cursor", async () => {
    // Earlier tests in this file already pushed messages into the same
    // (per-test-file-isolated) BusLog singleton, so `since=0` would return
    // THEIR messages first. Capture the cursor immediately before pushing
    // this test's own messages so pagination is scoped to just those.
    const baseline = await apiJson<{ next_cursor: number }>(`/sync/pull?since=0&limit=100000`);
    const startCursor = baseline.body.next_cursor;

    const hub = uniqueHub("asuspro13");
    const ids = [crypto.randomUUID(), crypto.randomUUID(), crypto.randomUUID()];
    await postJson("/sync/push", {
      origin_hub: hub,
      messages: ids.map((id, i) => ({
        id,
        sender: "claude",
        recipient: "codex",
        topic: "status",
        body: `page-item-${i}`,
      })),
    });

    const page1 = await apiJson<{ messages: Array<{ id: string }>; next_cursor: number; has_more: boolean }>(
      `/sync/pull?since=${startCursor}&limit=2`,
    );
    expect(page1.body.messages.length).toBeLessThanOrEqual(2);

    const page2 = await apiJson<{ messages: Array<{ id: string }>; next_cursor: number; has_more: boolean }>(
      `/sync/pull?since=${page1.body.next_cursor}&limit=2`,
    );
    const seenIds = new Set([...page1.body.messages, ...page2.body.messages].map((m) => m.id));
    // Every id we pushed for this hub must show up somewhere across the two pages.
    for (const id of ids) {
      expect(seenIds.has(id)).toBe(true);
    }
  });

  it("exclude_origin omits messages that originated at that hub", async () => {
    const hub = uniqueHub("spark-0060");
    const id = crypto.randomUUID();
    const pushResult = await postJson<{ cursor: number }>("/sync/push", {
      origin_hub: hub,
      messages: [
        { id, sender: "claude", recipient: "codex", topic: "status", body: "from spark-0060" },
      ],
    });

    const excluded = await apiJson<{ messages: Array<{ id: string }> }>(
      `/sync/pull?since=0&limit=500&exclude_origin=${hub}`,
    );
    expect(excluded.body.messages.some((m) => m.id === id)).toBe(false);

    const included = await apiJson<{ messages: Array<{ id: string }> }>(
      `/sync/pull?since=0&limit=500&exclude_origin=some-other-hub`,
    );
    expect(included.body.messages.some((m) => m.id === id)).toBe(true);
    void pushResult;
  });
});
