/**
 * Contract test (agent-hub#79 requirement): fixtures under `test/fixtures/`
 * are produced by `crates/agent-bus-core/tests/cloud_fixtures.rs`, which
 * serializes REAL Rust wire types (`Message`, `Presence`, `OwnershipClaim`,
 * `ArbitrationState`) with `serde_json::to_value`. This file proves the
 * Worker accepts and reproduces those exact JSON shapes — field names,
 * enum string values, and which optional fields are OMITTED vs. present-as-
 * `null` — rather than merely "looking similar" to the Rust output.
 *
 * Timestamps (`timestamp_utc`, `timestamp`, `expires_at`) are necessarily
 * dynamic (the fixture was generated at a fixed point in Rust-test time; the
 * Worker generates its own at request time), so those specific fields are
 * checked for FORMAT (the `%Y-%m-%dT%H:%M:%S%.6fZ` shape) rather than exact
 * value. Every other field is checked byte-for-byte against the fixture.
 */
import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import { formatTimestampUtc } from "../src/ids";
import messageFullFixture from "./fixtures/message_full.json";
import messageMinimalFixture from "./fixtures/message_minimal.json";
import presenceFullFixture from "./fixtures/presence_full.json";
import claimGrantedMinimalFixture from "./fixtures/claim_granted_minimal.json";
import claimContestedFullFixture from "./fixtures/claim_contested_full.json";
import arbitrationUnresolvedFixture from "./fixtures/arbitration_state_unresolved.json";

const ISO_6_DECIMAL = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{6}Z$/;

function busLogStub() {
  const id = env.BUS_LOG.idFromName(`contract-test-${crypto.randomUUID()}`);
  return env.BUS_LOG.get(id);
}

function claimStub(resource: string) {
  const id = env.CLAIM_DO.idFromName(resource);
  return env.CLAIM_DO.get(id);
}

describe("contract: Message", () => {
  it("round-trips every field of a fully-populated Rust Message verbatim", async () => {
    const stub = busLogStub();
    const fixture = messageFullFixture as unknown as Record<string, unknown>;
    const { message: storedMessage } = await stub.insertMessage({
      id: fixture.id as string,
      timestamp_utc: fixture.timestamp_utc as string,
      protocol_version: fixture.protocol_version as string,
      from: fixture.from as string,
      to: fixture.to as string,
      topic: fixture.topic as string,
      body: fixture.body as string,
      thread_id: fixture.thread_id as string,
      tags: fixture.tags as string[],
      priority: fixture.priority as string,
      request_ack: fixture.request_ack as boolean,
      reply_to: fixture.reply_to as string,
      metadata: fixture.metadata as never,
      stream_id: fixture.stream_id as string,
      // origin_hub (agent-hub#82 review M5): REQUIRED at insert time now
      // (the composite dedup key is `(origin_hub, id)`), but the Rust struct
      // this fixture was generated from has no such field at all — it is a
      // cloud-tier-only addition, excluded from the equality check below the
      // same way `stream_id` already is for the minimal fixture.
      origin_hub: "contract-test-hub",
    });
    const stored = storedMessage as unknown as Record<string, unknown>;
    const { origin_hub: _originHub, ...storedWithoutOriginHub } = stored;
    expect(storedWithoutOriginHub).toEqual(fixture);
    expect(typeof stored.origin_hub).toBe("string");
  });

  it("round-trips a minimal Rust Message: tags=[] and metadata=null are ALWAYS present, thread_id/reply_to are OMITTED", async () => {
    const stub = busLogStub();
    const fixture = messageMinimalFixture as unknown as Record<string, unknown>;
    const { message: storedMessage } = await stub.insertMessage({
      id: fixture.id as string,
      timestamp_utc: fixture.timestamp_utc as string,
      protocol_version: fixture.protocol_version as string,
      from: fixture.from as string,
      to: fixture.to as string,
      topic: fixture.topic as string,
      body: fixture.body as string,
      tags: fixture.tags as string[],
      priority: fixture.priority as string,
      request_ack: fixture.request_ack as boolean,
      metadata: fixture.metadata as never,
      origin_hub: "contract-test-hub",
    });
    const stored = storedMessage as unknown as Record<string, unknown>;
    // The fixture is a bare `serde_json::from_value` deserialize — it was
    // never posted through a bus, so its `stream_id` is genuinely `None`
    // (omitted). Once actually stored (by either hub), a message always
    // carries its log position, the same way Redis's XADD assigns one; that
    // is new information this specific field legitimately gains on write,
    // not a contract violation. `origin_hub` is excluded for the same reason
    // as the "fully-populated" test above. Every other field must still
    // match exactly.
    const { stream_id: _omittedFromFixture, origin_hub: _originHub, ...storedRest } = stored;
    expect(storedRest).toEqual(fixture);
    expect(typeof stored.stream_id).toBe("string");
    expect(typeof stored.origin_hub).toBe("string");
    expect(stored).not.toHaveProperty("thread_id");
    expect(stored).not.toHaveProperty("reply_to");
    expect(stored.tags).toEqual([]);
    expect(stored.metadata).toBeNull();
  });
});

describe("contract: Presence", () => {
  it("round-trips every field of a fully-populated Rust Presence", async () => {
    const stub = busLogStub();
    const fixture = presenceFullFixture as unknown as Record<string, unknown>;
    // `keyOrigin` (2nd arg, agent-hub#82 review M9) is an internal storage
    // discriminator, never serialized to the wire — the fixture (generated
    // from the Rust struct, which has no such concept) is unaffected as long
    // as `originHub` (3rd arg) is left unset, so `origin_hub` is never added
    // to the returned object.
    const stored = (await stub.setPresence(
      {
        agent: fixture.agent as string,
        status: fixture.status as string,
        protocol_version: fixture.protocol_version as string,
        timestamp_utc: fixture.timestamp_utc as string,
        session_id: fixture.session_id as string,
        capabilities: fixture.capabilities as string[],
        metadata: fixture.metadata as never,
        ttl_seconds: fixture.ttl_seconds as number,
      },
      "contract-test-key-origin",
    )) as unknown as Record<string, unknown>;
    expect(stored).toEqual(fixture);

    const listed = (await stub.listPresence()) as Array<Record<string, unknown>>;
    const found = listed.find((p) => p.agent === fixture.agent);
    expect(found).toEqual(fixture);
  });
});

describe("contract: OwnershipClaim", () => {
  it("a lone claim matches the granted-minimal fixture's field set", async () => {
    const fixture = claimGrantedMinimalFixture as unknown as Record<string, unknown>;
    const resource = `contract-claim-granted-${crypto.randomUUID()}`;
    const claim = (await claimStub(resource).claim(
      {
        resource,
        agent: fixture.agent as string,
        priorityArgument: fixture.priority_argument as string,
        mode: fixture.mode as "exclusive",
        leaseTtlSeconds: fixture.lease_ttl_seconds as number,
      },
      formatTimestampUtc(),
    )) as unknown as Record<string, unknown>;

    expect(claim.status).toBe(fixture.status);
    expect(claim.mode).toBe(fixture.mode);
    expect(claim.priority_argument).toBe(fixture.priority_argument);
    expect(claim.lease_ttl_seconds).toBe(fixture.lease_ttl_seconds);
    expect(claim.scope).toBe(fixture.scope);
    // Optional fields the fixture omits must be omitted here too.
    for (const key of ["namespace", "scope_kind", "scope_path", "thread_id", "repo_scopes", "reroute_suggestion"]) {
      expect(claim).not.toHaveProperty(key);
    }
    expect(claim.timestamp).toMatch(ISO_6_DECIMAL);
    expect(claim.expires_at).toMatch(ISO_6_DECIMAL);
    // Same key set as the fixture (module the dynamic timestamp fields, which
    // are still present under the same names on both sides).
    expect(Object.keys(claim).sort()).toEqual(Object.keys(fixture).sort());
  });

  it("a contested claim matches the contested-full fixture's field set and reroute_suggestion shape", async () => {
    const fixture = claimContestedFullFixture as unknown as Record<string, unknown>;
    const resource = `contract-claim-contested-${crypto.randomUUID()}`;
    const stub = claimStub(resource);

    await stub.claim(
      { resource, agent: "claude", priorityArgument: "first-edit required", mode: "exclusive" },
      formatTimestampUtc(),
    );
    const codexClaim = (await stub.claim(
      {
        resource,
        agent: fixture.agent as string,
        priorityArgument: fixture.priority_argument as string,
        mode: fixture.mode as "exclusive",
        namespace: fixture.namespace as string,
        scopeKind: fixture.scope_kind as string,
        scopePath: fixture.scope_path as string,
        repoScopes: fixture.repo_scopes as string[],
        threadId: fixture.thread_id as string,
        leaseTtlSeconds: fixture.lease_ttl_seconds as number,
        scope: fixture.scope as "machine",
      },
      formatTimestampUtc(),
    )) as unknown as Record<string, unknown>;

    expect(codexClaim.status).toBe("contested");
    expect(codexClaim.namespace).toBe(fixture.namespace);
    expect(codexClaim.scope_kind).toBe(fixture.scope_kind);
    expect(codexClaim.scope_path).toBe(fixture.scope_path);
    expect(codexClaim.repo_scopes).toEqual(fixture.repo_scopes);
    expect(codexClaim.thread_id).toBe(fixture.thread_id);
    expect(codexClaim.scope).toBe(fixture.scope);
    const reroute = codexClaim.reroute_suggestion as unknown as Record<string, unknown>;
    expect(reroute.original_resource).toBe(resource);
    expect(reroute.reason).toBe("resource contested by claude");
    expect(Object.keys(codexClaim).sort()).toEqual(Object.keys(fixture).sort());
    expect(Object.keys(reroute).sort()).toEqual(
      Object.keys(fixture.reroute_suggestion as object).sort(),
    );
  });
});

describe("contract: ArbitrationState", () => {
  it("an unresolved, never-claimed resource matches the fixture exactly (no dynamic fields at all)", async () => {
    const fixture = arbitrationUnresolvedFixture as unknown as Record<string, unknown>;
    const resource = `contract-arbitration-${crypto.randomUUID()}`;
    const state = await claimStub(resource).getState(resource);
    expect(state).toEqual({ ...fixture, resource });
  });

  it("winner/resolution_reason are ALWAYS present (never omitted), null when unresolved", async () => {
    const resource = `contract-arbitration-null-${crypto.randomUUID()}`;
    const state = (await claimStub(resource).getState(resource)) as unknown as Record<string, unknown>;
    expect(state).toHaveProperty("winner", null);
    expect(state).toHaveProperty("resolution_reason", null);
  });
});
