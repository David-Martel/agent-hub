import { describe, expect, it } from "vitest";
import { apiJson, postJson } from "./helpers";

describe("GET /notifications/:agent_id (inbox / check_inbox)", () => {
  it("lists messages addressed to the agent, newest first", async () => {
    await postJson("/messages", {
      sender: "claude",
      recipient: "inbox-test-agent",
      topic: "status",
      body: "first",
    });
    await postJson("/messages", {
      sender: "claude",
      recipient: "inbox-test-agent",
      topic: "status",
      body: "second",
    });

    const { status, body } = await apiJson<Array<{ message: { body: string }; requires_ack: boolean }>>(
      "/notifications/inbox-test-agent",
    );
    expect(status).toBe(200);
    expect(body.length).toBeGreaterThanOrEqual(2);
    expect(body[0]?.message.body).toBe("second");
  });

  it("marks requires_ack from the source message's request_ack flag", async () => {
    await postJson("/messages", {
      sender: "claude",
      recipient: "inbox-ack-agent",
      topic: "status",
      body: "please ack",
      request_ack: true,
    });
    const { body } = await apiJson<Array<{ requires_ack: boolean }>>("/notifications/inbox-ack-agent");
    expect(body[0]?.requires_ack).toBe(true);
  });

  it("supports since_id pagination", async () => {
    await postJson("/messages", { sender: "claude", recipient: "inbox-page-agent", topic: "status", body: "a" });
    const first = await apiJson<Array<{ id: string }>>("/notifications/inbox-page-agent?history=1");
    const sinceId = first.body[0]?.id;
    await postJson("/messages", { sender: "claude", recipient: "inbox-page-agent", topic: "status", body: "b" });
    const second = await apiJson<Array<{ message: { body: string } }>>(
      `/notifications/inbox-page-agent?since_id=${sinceId}`,
    );
    expect(second.body.every((n) => n.message.body !== "a")).toBe(true);
    expect(second.body.some((n) => n.message.body === "b")).toBe(true);
  });
});

describe("GET /pending-acks", () => {
  it("lists messages awaiting acknowledgement", async () => {
    await postJson("/messages", {
      sender: "claude",
      recipient: "pending-ack-agent",
      topic: "status",
      body: "needs ack",
      request_ack: true,
    });
    const { status, body } = await apiJson<Array<{ recipient: string; stale: boolean }>>(
      "/pending-acks?agent=pending-ack-agent",
    );
    expect(status).toBe(200);
    expect(body.length).toBeGreaterThanOrEqual(1);
    expect(body[0]?.recipient).toBe("pending-ack-agent");
    expect(body[0]?.stale).toBe(false);
  });
});
