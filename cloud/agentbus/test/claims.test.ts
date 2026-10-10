import { env } from "cloudflare:workers";
import { runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { normalizeResourceName } from "../src/claims-logic";
import { api, apiJson, CLAUDE_TOKEN, HUB_A_TOKEN, OPERATOR_TOKEN, postJson, putJson } from "./helpers";

function resource(): string { return "authority-" + crypto.randomUUID(); }
function stubFor(name: string) {
  return env.CLAIM_DO.get(env.CLAIM_DO.idFromName(normalizeResourceName(name)));
}

async function retainedSnapshot(name: string) {
  return runInDurableObject(stubFor(name), (_instance, state) => ({
    claims: state.storage.sql.exec("SELECT * FROM claims ORDER BY agent").toArray(),
    resolution: state.storage.sql.exec("SELECT * FROM resolution ORDER BY id").toArray(),
    events: state.storage.sql.exec("SELECT * FROM events ORDER BY seq").toArray(),
  }));
}

async function seed(name: string) {
  await stubFor(name).claim({
    resource: name, agent: "claude", priorityArgument: "retained fixture",
    mode: "exclusive", leaseTtlSeconds: 3600,
  }, new Date().toISOString());
  await stubFor(name).resolve(name, "claude", "retained resolution", "operator");
}

const mutations = [
  { method: "POST", suffix: "", body: { agent: "claude", priority_argument: "new request" } },
  { method: "POST", suffix: "/renew", body: { agent: "claude", lease_ttl_seconds: 7200 } },
  { method: "POST", suffix: "/release", body: { agent: "claude" } },
] as const;

describe("Cloud claim mutations retain the on-site authority", () => {
  for (const [role, token] of [
    ["agent", CLAUDE_TOKEN], ["hub", HUB_A_TOKEN], ["operator", OPERATOR_TOKEN],
  ] as const) {
    for (const mutation of mutations) {
      it(role + " " + mutation.suffix + " returns 409 without changing retained DO rows", async () => {
        const name = resource();
        await seed(name);
        const before = await retainedSnapshot(name);
        const response = await api("/channels/arbitrate/" + name + mutation.suffix, {
          method: mutation.method, body: JSON.stringify(mutation.body),
        }, token);
        expect(response.status).toBe(409);
        expect(response.headers.get("cache-control")).toBe("no-store");
        expect(await response.json()).toEqual({
          error: "cloud claims are read-only; use the on-site claims authority",
        });
        expect(await retainedSnapshot(name)).toEqual(before);
      });
    }
  }

  it("operator resolve returns 409 without changing claims, resolution or event rows", async () => {
    const name = resource();
    await seed(name);
    const before = await retainedSnapshot(name);
    const result = await putJson("/channels/arbitrate/" + name + "/resolve", {
      winner: "other-agent", reason: "would replace retained resolution", resolved_by: "operator",
    }, OPERATOR_TOKEN);
    expect(result.status).toBe(409);
    expect(await retainedSnapshot(name)).toEqual(before);
  });

  it("refused mutations do not prune an expired retained claim", async () => {
    const name = resource();
    await seed(name);
    await runInDurableObject(stubFor(name), (_instance, state) => {
      state.storage.sql.exec("UPDATE claims SET expires_at = ?", "2000-01-01T00:00:00.000000Z");
    });
    const before = await retainedSnapshot(name);
    for (const mutation of mutations) {
      expect((await postJson("/channels/arbitrate/" + name + mutation.suffix, mutation.body)).status).toBe(409);
    }
    expect((await putJson("/channels/arbitrate/" + name + "/resolve", {
      winner: "claude",
    }, OPERATOR_TOKEN)).status).toBe(409);
    expect(await retainedSnapshot(name)).toEqual(before);
  });

  it("does not create ClaimDO tables for a never-claimed resource", async () => {
    const name = resource();
    for (const mutation of mutations) {
      expect((await postJson("/channels/arbitrate/" + name + mutation.suffix, mutation.body)).status).toBe(409);
    }
    expect((await putJson("/channels/arbitrate/" + name + "/resolve", {
      winner: "claude",
    }, OPERATOR_TOKEN)).status).toBe(409);
    const tables = await runInDurableObject(stubFor(name), (_instance, state) =>
      state.storage.sql.exec("SELECT name FROM sqlite_master WHERE type = 'table'").toArray());
    expect(tables).toEqual([]);
  });
});

describe("Existing authentication and validation precede authority refusal", () => {
  for (const mutation of [
    ...mutations, { method: "PUT", suffix: "/resolve", body: { winner: "claude" } },
  ]) {
    it("unauthenticated " + mutation.suffix + " remains 401", async () => {
      const response = await api("/channels/arbitrate/" + resource() + mutation.suffix, {
        method: mutation.method, body: JSON.stringify(mutation.body),
      }, null);
      expect(response.status).toBe(401);
    });
  }

  for (const mutation of mutations) {
    it("mismatched agent " + mutation.suffix + " remains 403", async () => {
      expect((await postJson("/channels/arbitrate/" + resource() + mutation.suffix, {
        ...mutation.body, agent: "codex",
      }, CLAUDE_TOKEN)).status).toBe(403);
    });
  }

  for (const token of [CLAUDE_TOKEN, HUB_A_TOKEN]) {
    it("non-operator resolve remains 403", async () => {
      expect((await putJson("/channels/arbitrate/" + resource() + "/resolve", {
        winner: "claude",
      }, token)).status).toBe(403);
    });
  }

  it("rejects invalid claim fields and missing shared namespace with 400", async () => {
    for (const body of [
      { mode: "invalid" }, { scope: "invalid" }, { namespace: {} },
      { repo_scopes: "invalid" }, { mode: "shared_namespaced" },
      { lease_ttl_seconds: "invalid" },
    ]) {
      expect((await postJson("/channels/arbitrate/" + resource(), {
        agent: "claude", ...body,
      })).status).toBe(400);
    }
  });

  it("rejects invalid renewal TTL, empty winner and oversized resource before refusal", async () => {
    expect((await postJson("/channels/arbitrate/" + resource() + "/renew", {
      agent: "claude", lease_ttl_seconds: "invalid",
    })).status).toBe(400);
    expect((await putJson("/channels/arbitrate/" + resource() + "/resolve", {
      winner: "",
    }, OPERATOR_TOKEN)).status).toBe(400);
    const response = await api("/channels/arbitrate/" + "x".repeat(300), {
      method: "POST", body: JSON.stringify({ agent: "claude" }),
    });
    expect(response.status).toBe(400);
    expect(response.headers.get("content-type")).toMatch(/application\/json/);
    expect(response.headers.get("cache-control")).toBe("no-store");
  });
});

describe("Cloud reads expose retained Cloud state, not on-site ownership", () => {
  it("keeps the existing state and event JSON shapes for retained claims", async () => {
    const name = resource();
    await seed(name);
    const state = await apiJson<{ resource: string; claims: unknown[]; winner: string }>(
      "/channels/arbitrate/" + name);
    expect(state.status).toBe(200);
    expect(state.body.resource).toBe(name);
    expect(state.body.claims).toHaveLength(1);
    expect(state.body.winner).toBe("claude");
    const events = await apiJson<Array<{ event: string }>>("/resource-events/" + name);
    expect(events.status).toBe(200);
    expect(events.body.map(event => event.event)).toEqual(["resolved", "claimed"]);
  });

  it("keeps empty-state reads and their authentication requirement", async () => {
    const name = resource();
    expect((await apiJson("/channels/arbitrate/" + name)).body).toEqual({
      resource: name, claims: [], winner: null, resolution_reason: null,
    });
    expect((await apiJson("/resource-events/" + name)).body).toEqual([]);
    for (const path of ["/channels/arbitrate/" + name, "/resource-events/" + name]) {
      expect((await api(path, undefined, null)).status).toBe(401);
    }
  });
});
