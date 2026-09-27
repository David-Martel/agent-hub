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

describe("smoke: insertMessageBatch rolls back atomically on a storage-path exception (re-review N8)", () => {
  it("a genuine storage error (not a validation error) mid-batch rolls back everything already inserted in the SAME call", async () => {
    // This is the load-bearing atomicity claim behind the N8 fix: index.ts
    // validates every item BEFORE calling insertMessageBatch, so the ONLY
    // way item 0 could still end up persisted while item 1 fails is a
    // genuine storage-path exception thrown partway through this single
    // synchronous DO RPC. insertMessage() itself throws exactly that shape
    // of error when origin_hub is missing (see do-buslog.ts) -- a case that
    // can't happen via the real /messages/batch route (bindOriginHubForDirectWrite
    // always supplies one), but is the most direct way to prove the
    // rollback property empirically rather than merely asserting it in a
    // comment.
    const id = env.BUS_LOG.idFromName(`batch-rollback-${crypto.randomUUID()}`);
    const stub = env.BUS_LOG.get(id);
    const recipient = `batch-rollback-agent-${crypto.randomUUID()}`;
    const validItem = {
      id: crypto.randomUUID(),
      origin_hub: "asuspro13",
      timestamp_utc: "2026-09-27T00:00:00.000000Z",
      protocol_version: "1.0",
      from: "claude",
      to: recipient,
      topic: "status",
      body: "should be rolled back",
      tags: [],
      priority: "normal",
      request_ack: false,
      metadata: {},
    };
    // origin_hub omitted entirely (cast around the type since the real
    // caller can never construct this) so insertMessage throws INSIDE the
    // batch, after the first item already ran its INSERT.
    const brokenItem = { ...validItem, id: crypto.randomUUID(), origin_hub: undefined } as unknown as typeof validItem;

    let threw = false;
    try {
      await stub.insertMessageBatch([validItem, brokenItem]);
    } catch {
      threw = true;
    }
    expect(threw).toBe(true);

    const rows = await stub.listMessages({
      agent: recipient,
      since_ms: 0,
      limit: 10,
      include_broadcast: true,
    });
    expect(rows.some((m) => m.id === validItem.id)).toBe(false);
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

  it("scheduling is idempotent: a second call does not push the alarm further into the future (re-review N5)", async () => {
    // maybeScheduleRetention takes retentionDays as a PARAMETER (not read
    // from env), so this exercises it directly without needing the
    // process-wide RETENTION_DAYS binding (unset in vitest.config.ts).
    const id = env.BUS_LOG.idFromName(`retention-idempotent-${crypto.randomUUID()}`);
    const stub = env.BUS_LOG.get(id);

    await stub.maybeScheduleRetention("90");
    const first = await stub.debugGetAlarmTime();
    expect(first).not.toBeNull();

    // The old code unconditionally called setAlarm() again here, pushing
    // the alarm 24h further into the future every time -- which meant a DO
    // instance that gets evicted/reconstructed often (ensureSchema() calls
    // this on every fresh instance) could see the alarm's due time keep
    // sliding forward and never actually fire.
    await stub.maybeScheduleRetention("90");
    const second = await stub.debugGetAlarmTime();
    expect(second).toBe(first);
  });

  it("a historical (e.g. June) import survives RETENTION_DAYS=90 because pruning keys on cloud INGEST time, not the message's own timestamp_utc (re-review N5)", async () => {
    const id = env.BUS_LOG.idFromName(`retention-june-import-${crypto.randomUUID()}`);
    const stub = env.BUS_LOG.get(id);

    // A message "authored" in June -- well past a 90-day window measured
    // from its own timestamp_utc, but ingested into THIS store just now
    // (insertMessage always stamps ingested_at_utc with the real current
    // time, by design -- see do-buslog.ts).
    const juneMessageId = crypto.randomUUID();
    const { inserted } = await stub.insertMessage({
      id: juneMessageId,
      origin_hub: "asuspro13",
      timestamp_utc: "2026-06-01T00:00:00.000000Z",
      protocol_version: "1.0",
      from: "codex",
      to: "claude",
      topic: "status",
      body: "a June-dated historical import",
      tags: [],
      priority: "normal",
      request_ack: false,
      metadata: {},
    });
    expect(inserted).toBe(true);

    // Exercise the real production code path: this.env.RETENTION_DAYS,
    // read fresh by alarm() on every invocation. `env` here is the exact
    // object the Workers runtime hands to every Durable Object
    // construction in this isolate, so mutating it is visible to
    // `this.env` inside the DO.
    const previousRetentionDays = env.RETENTION_DAYS;
    env.RETENTION_DAYS = "90";
    try {
      await stub.maybeScheduleRetention("90");
      await runDurableObjectAlarm(stub);
    } finally {
      env.RETENTION_DAYS = previousRetentionDays;
    }

    const survivors = await stub.listMessages({
      agent: "claude",
      since_ms: Date.parse("2026-01-01T00:00:00Z"),
      limit: 100,
      include_broadcast: true,
    });
    expect(survivors.some((m) => m.id === juneMessageId)).toBe(true);
  });
});
