import { env } from "cloudflare:workers";
import { runInDurableObject } from "cloudflare:test";
import { describe, expect, it } from "vitest";
import { normalizeResourceName } from "../src/claims-logic";

// Retained DO implementation coverage for a future coordinated migration.
// These direct RPC fixtures do not imply the Cloud HTTP API may grant claims.
function fixture() {
  const resource = "retained-do-" + crypto.randomUUID();
  const stub = env.CLAIM_DO.get(env.CLAIM_DO.idFromName(resource));
  const claim = (agent: string, mode: "exclusive" | "shared" | "shared_namespaced" = "exclusive",
    namespace?: string, ttl = 3600) => stub.claim({
    resource, agent, priorityArgument: "fixture", mode, namespace, leaseTtlSeconds: ttl,
  }, new Date().toISOString());
  return { resource, stub, claim };
}

describe("retained ClaimDO implementation, direct disposable RPC only", () => {
  it("grants one exclusive claim and contests concurrent exclusive claims", async () => {
    const { resource, stub, claim } = fixture();
    const replies = await Promise.all([claim("claude"), claim("codex")]);
    expect(replies.map(reply => reply.status).sort()).toEqual(["contested", "granted"]);
    const state = await stub.getState(resource);
    expect(state.claims).toHaveLength(2);
    expect(state.claims.every(value => value.status === "contested")).toBe(true);
    expect(replies.find(value => value.status === "contested")?.reroute_suggestion?.original_resource).toBe(resource);
  });

  it("allows shared and distinct namespaces but rejects missing shared namespace", async () => {
    const shared = fixture();
    expect((await shared.claim("claude", "shared")).status).toBe("granted");
    expect((await shared.claim("codex", "shared")).status).toBe("granted");
    const scoped = fixture();
    expect((await scoped.claim("claude", "shared_namespaced", "a")).status).toBe("granted");
    expect((await scoped.claim("codex", "shared_namespaced", "b")).status).toBe("granted");
    const missingNamespace = await runInDurableObject(scoped.stub, async instance => {
      try {
        await instance.claim({
          resource: scoped.resource, agent: "missing", priorityArgument: "fixture",
          mode: "shared_namespaced", leaseTtlSeconds: 3600,
        }, new Date().toISOString());
        return "unexpected success";
      } catch (error) {
        return error instanceof Error ? error.message : String(error);
      }
    });
    expect(missingNamespace).toMatch(/namespace/);
  });

  it("retains machine-global reroute suggestions", async () => {
    const resource = "cargo-target-" + crypto.randomUUID();
    const stub = env.CLAIM_DO.get(env.CLAIM_DO.idFromName(resource));
    await stub.claim({ resource, agent: "claude", priorityArgument: "", mode: "exclusive" }, new Date().toISOString());
    const result = await stub.claim({
      resource, agent: "codex", priorityArgument: "", mode: "exclusive",
    }, new Date().toISOString());
    expect(result.reroute_suggestion?.suggested_resource).toBe("cargo-target:codex");
  });

  it("retains resolution, renewal, release and original lifecycle events", async () => {
    const { resource, stub, claim } = fixture();
    await claim("claude");
    await claim("codex");
    const resolved = await stub.resolve(resource, "claude", "fixture resolution", "operator");
    expect(resolved.winner).toBe("claude");
    expect(resolved.claims.find(value => value.agent === "codex")?.status).toBe("review_assigned");
    expect((await stub.renew(resource, "claude", 7200, new Date().toISOString())).lease_ttl_seconds).toBe(7200);
    expect((await stub.release(resource, "codex")).claims.map(value => value.agent)).toEqual(["claude"]);
    expect((await stub.listEvents()).map(value => value.event)).toEqual([
      "released", "renewed", "resolved", "contested", "claimed",
    ]);
  });

  it("retains missing-claim errors inside the direct implementation", async () => {
    const { resource, stub } = fixture();
    const errors = await runInDurableObject(stub, async instance => {
      const messages: string[] = [];
      for (const operation of [
        () => instance.renew(resource, "claude", undefined, new Date().toISOString()),
        () => instance.release(resource, "claude"),
        () => instance.resolve(resource, "claude", "fixture", "operator"),
      ]) {
        try {
          await operation();
          messages.push("unexpected success");
        } catch (error) {
          messages.push(error instanceof Error ? error.message : String(error));
        }
      }
      return messages;
    });
    expect(errors).toHaveLength(3);
    expect(errors[0]).toMatch(/no active claim/);
    expect(errors[1]).toMatch(/no active claim/);
    expect(errors[2]).toMatch(/no claims found/);
  });

  it("caps lease TTL and retains existing read-triggered expiry housekeeping", async () => {
    const { resource, stub, claim } = fixture();
    expect((await claim("claude", "exclusive", undefined, 1e13)).lease_ttl_seconds).toBe(86400);
    await runInDurableObject(stub, (_instance, state) => {
      state.storage.sql.exec("UPDATE claims SET expires_at = ?", "2000-01-01T00:00:00.000000Z");
    });
    expect((await stub.getState(resource)).claims).toEqual([]);
    const rows = await runInDurableObject(stub, (_instance, state) =>
      state.storage.sql.exec("SELECT * FROM claims").toArray());
    expect(rows).toEqual([]);
    expect((await stub.listEvents()).map(value => value.event)).toEqual(["claimed"]);
  });

  it("keeps case/backslash normalization and empty read shapes", async () => {
    const unique = crypto.randomUUID();
    expect(normalizeResourceName("A\\" + unique + "\\B")).toBe(normalizeResourceName("a/" + unique + "/b"));
    const { resource, stub } = fixture();
    expect(await stub.getState(resource)).toEqual({
      resource, claims: [], winner: null, resolution_reason: null,
    });
    expect(await stub.listEvents()).toEqual([]);
  });
});
