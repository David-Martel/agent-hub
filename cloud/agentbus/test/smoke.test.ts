import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";

describe("smoke: DO RPC + SQLite storage backend", () => {
  it("BusLog stub answers an RPC call", async () => {
    const id = env.BUS_LOG.idFromName("smoke-test");
    const stub = env.BUS_LOG.get(id);
    const stats = await stub.stats();
    expect(stats).toEqual({ message_count: 0, presence_count: 0 });
  });

  it("ClaimDO stub answers an RPC call", async () => {
    const id = env.CLAIM_DO.idFromName("smoke-test-resource");
    const stub = env.CLAIM_DO.get(id);
    const state = await stub.getState("smoke-test-resource");
    expect(state).toEqual({
      resource: "smoke-test-resource",
      claims: [],
      winner: null,
      resolution_reason: null,
    });
  });
});
