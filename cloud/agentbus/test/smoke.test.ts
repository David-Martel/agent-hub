import { env } from "cloudflare:workers";
import { runDurableObjectAlarm } from "cloudflare:test";
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

describe("smoke: rate limiting (agent-hub#82, checkRateLimit)", () => {
  it("allows up to the limit, then denies within the same 60s window", async () => {
    const id = env.BUS_LOG.idFromName("global");
    const stub = env.BUS_LOG.get(id);
    const key = `rate-limit-smoke-${crypto.randomUUID()}`;
    expect(await stub.checkRateLimit(key, 2)).toBe(true);
    expect(await stub.checkRateLimit(key, 2)).toBe(true);
    expect(await stub.checkRateLimit(key, 2)).toBe(false);
  });

  it("a different identity key has its own independent budget", async () => {
    const id = env.BUS_LOG.idFromName("global");
    const stub = env.BUS_LOG.get(id);
    const keyA = `rate-limit-smoke-a-${crypto.randomUUID()}`;
    const keyB = `rate-limit-smoke-b-${crypto.randomUUID()}`;
    expect(await stub.checkRateLimit(keyA, 1)).toBe(true);
    expect(await stub.checkRateLimit(keyA, 1)).toBe(false);
    expect(await stub.checkRateLimit(keyB, 1)).toBe(true);
  });
});

describe("smoke: retention alarm (agent-hub#82, off by default)", () => {
  it("maybeScheduleRetention is a no-op when RETENTION_DAYS is unset", async () => {
    // This Worker's test bindings (vitest.config.ts) never set
    // RETENTION_DAYS, matching the "off by default" requirement. Calling the
    // scheduler directly with an unset/invalid value must not throw and must
    // not schedule an alarm.
    const id = env.BUS_LOG.idFromName(`retention-smoke-${crypto.randomUUID()}`);
    const stub = env.BUS_LOG.get(id);
    // Any RPC call runs `ensureSchema()`, which calls `maybeScheduleRetention`
    // internally with `this.env.RETENTION_DAYS` (unset in this test env).
    await stub.stats();
    // No alarm was ever scheduled (RETENTION_DAYS unset), so there is
    // nothing for the test harness to run — this proves the "off by
    // default" behavior rather than the alarm handler's internal logic.
    await expect(runDurableObjectAlarm(stub)).resolves.toBe(false);
  });
});
