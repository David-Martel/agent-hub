import { env } from "cloudflare:workers";
import { runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import type { InsertMessageInput, ReadMessagesQuery } from "../src/do-buslog";

describe("message query index admission", () => {
  it("preserves optional filters, broadcast, tags and backward pagination", async () => {
    const stub = env.BUS_LOG.get(env.BUS_LOG.idFromName(`query-semantics-${crypto.randomUUID()}`));
    await runInDurableObject(stub, (instance) => {
      const fixtures: InsertMessageInput[] = [];
      for (let i = 0; i < 650; i++) {
        const input: InsertMessageInput = { id: `fixture-${i}`, origin_hub: "query-fixture", timestamp_utc: new Date(1_700_000_000_000 + i).toISOString(),
          protocol_version: "1.0", from: i % 4 ? "sender-a" : "sender-b", to: i % 3 ? "recipient" : "all",
          topic: i % 5 ? "status" : "claim", thread_id: i % 7 ? "thread-a" : "thread-b", body: "synthetic",
          tags: i < 350 ? ["repo:fixture", "session:fixture", "rare"] : [], priority: "normal", request_ack: false, metadata: {} };
        fixtures.push(input);
        instance.insertMessage(input);
      }
      const base: ReadMessagesQuery = { since_ms: 0, limit: 21, include_broadcast: false };
      const cases: ReadMessagesQuery[] = [base,
        { ...base, agent: "recipient" }, { ...base, agent: "recipient", include_broadcast: true },
        { ...base, agent: "all", include_broadcast: true }, { ...base, agent: "missing" },
        { ...base, from_agent: "sender-b" }, { ...base, agent: "recipient", from_agent: "sender-b", topic: "claim", thread_id: "thread-b" },
        { ...base, agent: "recipient", include_broadcast: true, required_tags: ["rare"], repo: "fixture", session: "fixture" },
        { ...base, agent: "recipient", required_tags: ["absent"] },
        { ...base, agent: "recipient", since_ms: 1_700_000_000_600 },
        { ...base, agent: "" }, { ...base, topic: "status' OR 1=1 --" },
      ];
      for (const query of cases) {
        const tags = [...(query.required_tags ?? []), ...(query.repo ? [`repo:${query.repo}`] : []), ...(query.session ? [`session:${query.session}`] : [])];
        // Independent in-memory selection over ordered synthetic input: no
        // duplicated SQL or production implementation in the expectation.
        const expected = fixtures.filter(row => Date.parse(row.timestamp_utc) >= query.since_ms
          && (query.from_agent == null || row.from === query.from_agent)
          && (query.agent == null || row.to === query.agent || (query.include_broadcast && row.to === "all"))
          && (query.topic == null || row.topic === query.topic)
          && (query.thread_id == null || row.thread_id === query.thread_id)
          && tags.every(tag => row.tags.includes(tag))).slice(-query.limit).map(row => row.id);
        expect(instance.listMessages(query).map(row => row.id)).toEqual(expected);
      }
    });
  });

  it("preserves batch composite origin/id deduplication and chronological order", async () => {
    const stub = env.BUS_LOG.get(env.BUS_LOG.idFromName(`query-batch-${crypto.randomUUID()}`));
    await runInDurableObject(stub, async (instance) => {
      for (const [index, origin, recipient] of [[0, "origin-a", "recipient"], [1, "origin-b", "recipient"], [2, "origin-c", "all"]] as const) {
        instance.insertMessage({ id: "same-id", origin_hub: origin, timestamp_utc: new Date(1_700_000_000_000 + index).toISOString(), protocol_version: "1.0",
          from: "fixture", to: recipient, topic: "status", body: "synthetic", tags: [], priority: "normal", request_ack: false, metadata: {} });
      }
      const query: ReadMessagesQuery = { agent: "recipient", since_ms: 0, limit: 10, include_broadcast: true };
      const actual = await instance.listMessagesBatch([query, query]).json();
      expect(actual).toHaveLength(3);
      expect((actual as {origin_hub: string}[]).map(row => row.origin_hub)).toEqual(["origin-a", "origin-b", "origin-c"]);
    });
  });

  it("reads sparse recipient rows through existing indexes in the actual BusLog method", async () => {
    const stub = env.BUS_LOG.get(env.BUS_LOG.idFromName(`query-native-cost-${crypto.randomUUID()}`));
    await runInDurableObject(stub, (instance, state) => {
      instance.currentCursor();
      for (let i = 0; i < 3000; i++) state.storage.sql.exec(
        "INSERT INTO messages (origin_hub,id,timestamp_utc,protocol_version,from_agent,to_agent,topic,body,tags,priority,request_ack) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        "synthetic", `fixture-${i}`, "2026-10-15T00:00:00.000Z", "1.0", "fixture", `recipient-${i % 1000}`, "status", "synthetic", "[]", "normal", 0);
      const query: ReadMessagesQuery = { agent: "recipient-1", since_ms: 0, limit: 20, include_broadcast: false };
      let rowsRead = 0;
      const target = instance as unknown as {sql: SqlStorage};
      const original = target.sql;
      target.sql = new Proxy(original, { get(sql, key) {
        if (key !== "exec") return Reflect.get(sql, key, sql);
        return (statement: string, ...bindings: SqlStorageValue[]) => {
          const cursor = sql.exec(statement, ...bindings);
          return new Proxy(cursor, { get(value, property) {
            if (property !== "toArray") return Reflect.get(value, property, value);
            return () => { const result = value.toArray(); rowsRead += value.rowsRead; return result; };
          }});
        };
      }});
      let actual;
      try { actual = instance.listMessages(query); } finally { target.sql = original; }
      expect(actual.map(row => row.id)).toEqual(["fixture-1", "fixture-1001", "fixture-2001"]);
      expect(actual).toHaveLength(3);
      expect(rowsRead).toBeLessThan(50);
      console.log(JSON.stringify({ event: "synthetic_buslog_query_cost", fixture_rows: 3000, returned: actual.length, rowsRead }));
    });
  });
});
