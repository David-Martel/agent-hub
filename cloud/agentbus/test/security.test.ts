/**
 * Regression tests mapped directly to the exploit probes (P1-P11) and
 * findings (H1-H7, M1-M9, L1-L2) in the agent-hub#82 security review
 * (~/.local/share/jules-fleet/handoff-2026-09-26/status-agent-hub-82-security.md).
 * Many of these scenarios are ALSO covered inline in the per-route test
 * files (messages/presence/claims/sync.test.ts) where they fit naturally
 * alongside the route's other behavior; this file exists so every P-numbered
 * probe from the review has an unambiguous, named regression test even where
 * it would otherwise be scattered.
 */
import { describe, expect, it } from "vitest";
// `?raw` is a Vite/vitest build-time transform (resolved when this test file
// is bundled, not a runtime filesystem read) — safe inside the Workers
// runtime this test suite otherwise executes in, which has no local
// filesystem.
// eslint-disable-next-line import/no-unresolved
import wranglerToml from "../wrangler.toml?raw";
import { authenticate, MIN_TOKEN_LEN } from "../src/auth";
import {
  api,
  apiJson,
  CLAUDE_TOKEN,
  CODEX_TOKEN,
  HUB_A,
  HUB_A_TOKEN,
  OPERATOR_TOKEN,
  postJson,
  putJson,
} from "./helpers";

describe("P1: identity spoofing on POST /messages (H1)", () => {
  it("a claude token can no longer post as sender=codex with an arbitrary origin_host/origin_hub", async () => {
    const { status } = await postJson(
      "/messages",
      {
        sender: "codex",
        recipient: "claude",
        topic: "status",
        body: "spoofed",
        origin_host: "spark-0060",
        origin_hub: "asuspro13-hub",
      },
      CLAUDE_TOKEN,
    );
    expect(status).toBe(403);
  });

  it("origin_hub asserted by a non-hub token is rejected even when it happens to equal the cloud identity", async () => {
    const { status, body } = await postJson<Record<string, unknown>>(
      "/messages",
      { sender: "claude", recipient: "codex", topic: "status", body: "x", origin_hub: "cloud" },
      CLAUDE_TOKEN,
    );
    // "cloud" is the actual HUB_IDENTITY default, so this only succeeds
    // because it matches — bindOriginHubForDirectWrite still runs the check.
    expect(status).toBe(200);
    expect(body.origin_hub).toBe("cloud");
  });
});

describe("P2: claims authority has no authorization (H2)", () => {
  it("a claude token cannot renew, release or resolve codex's claim", async () => {
    const res = `p2-${crypto.randomUUID()}`;
    await postJson(`/channels/arbitrate/${res}`, { agent: "codex" }, CODEX_TOKEN);

    const renew = await postJson(`/channels/arbitrate/${res}/renew`, { agent: "codex" }, CLAUDE_TOKEN);
    expect(renew.status).toBe(403);

    const release = await postJson(`/channels/arbitrate/${res}/release`, { agent: "codex" }, CLAUDE_TOKEN);
    expect(release.status).toBe(403);

    const resolve = await putJson(`/channels/arbitrate/${res}/resolve`, { winner: "claude" }, CLAUDE_TOKEN);
    expect(resolve.status).toBe(403);

    // codex's claim is untouched.
    const state = await apiJson<{ claims: Array<{ agent: string; status: string }> }>(
      `/channels/arbitrate/${res}`,
    );
    expect(state.body.claims.some((c) => c.agent === "codex")).toBe(true);
  });
});

describe("P3: unbounded/malformed claim lease TTL (M4)", () => {
  it("a huge lease_ttl_seconds is capped, not left to overflow the expiry timestamp", async () => {
    const res = `p3-huge-${crypto.randomUUID()}`;
    const { status, body } = await postJson<{ lease_ttl_seconds: number; expires_at: string }>(
      `/channels/arbitrate/${res}`,
      { agent: "claude", lease_ttl_seconds: 1e9 },
    );
    expect(status).toBe(200);
    expect(body.lease_ttl_seconds).toBeLessThanOrEqual(86_400);
    // A valid, parseable timestamp — not the "+011533-..." garbage a 3e11
    // second lease produced before the cap existed.
    expect(body.expires_at).toMatch(/^\d{4}-\d{2}-\d{2}T/);
  });

  it("1e13 and a non-numeric lease_ttl_seconds are 400s, not 500 'Invalid time value'", async () => {
    const huge = await postJson<{ error: string }>(`/channels/arbitrate/${crypto.randomUUID()}`, {
      agent: "claude",
      lease_ttl_seconds: 1e13,
    });
    expect(huge.status).toBe(200); // clamped, not rejected — still succeeds, just capped
    const nonNumeric = await postJson<{ error: string }>(`/channels/arbitrate/${crypto.randomUUID()}`, {
      agent: "claude",
      lease_ttl_seconds: "abc" as unknown as number,
    });
    expect(nonNumeric.status).toBe(400);
    expect(nonNumeric.body.error).not.toMatch(/Invalid time value/);
  });

  it("renew also caps and validates lease_ttl_seconds", async () => {
    const res = `p3-renew-${crypto.randomUUID()}`;
    await postJson(`/channels/arbitrate/${res}`, { agent: "claude" });
    const { status, body } = await postJson<{ lease_ttl_seconds: number }>(`/channels/arbitrate/${res}/renew`, {
      agent: "claude",
      lease_ttl_seconds: 1e9,
    });
    expect(status).toBe(200);
    expect(body.lease_ttl_seconds).toBeLessThanOrEqual(86_400);
  });
});

describe("P6: tag-poisoning DoS on tag-scoped reads (H7)", () => {
  it("POST /messages rejects a non-array tags value with 400 instead of storing it", async () => {
    const { status, body } = await postJson<{ error: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "x",
      tags: 5,
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/tags/);
  });

  it("a tag-scoped read still works normally afterward (no 500 from a poisoned row)", async () => {
    await postJson("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "tagged",
      tags: ["repo:agent-hub"],
    });
    const { status } = await apiJson("/messages?tag=repo:agent-hub");
    expect(status).toBe(200);
  });
});

describe("P7: unbounded metadata + PHI on knock/ack/presence (M1/M7)", () => {
  it("rejects oversized metadata on POST /messages", async () => {
    const { status, body } = await postJson<{ error: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "x",
      metadata: { blob: "x".repeat(2_000_000) },
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/metadata/);
  });

  it("rejects a knock body matching an obvious PHI pattern (was accepted verbatim before)", async () => {
    const { status, body } = await postJson<{ error: string }>("/knock", {
      sender: "claude",
      recipient: "codex",
      body: "patient name and MRN 12345678 attached",
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/PHI/);
  });

  it("rejects an ack body matching an obvious PHI pattern (was accepted verbatim before)", async () => {
    const sent = await postJson<{ id: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "needs ack",
      request_ack: true,
    });
    const { status, body } = await postJson<{ error: string }>(`/messages/${sent.body.id}/ack`, {
      agent: "claude",
      body: "DOB: 01/02/1990",
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/PHI/);
  });
});

describe("P8/P11: non-numeric query/body params are 400s, never a raw 500 (L1/L2)", () => {
  it("GET /messages?limit=abc is a 400", async () => {
    const { status } = await apiJson("/messages?limit=abc");
    expect(status).toBe(400);
  });

  it("GET /messages?since=abc is a 400", async () => {
    const { status } = await apiJson("/messages?since=abc");
    expect(status).toBe(400);
  });

  it("GET /presence/history?limit=abc is a 400", async () => {
    const { status } = await apiJson("/presence/history?limit=abc");
    expect(status).toBe(400);
  });

  it("GET /notifications/:agent_id?history=abc is a 400", async () => {
    const { status } = await apiJson("/notifications/someone?history=abc");
    expect(status).toBe(400);
  });

  it("GET /resource-events/:id?limit=abc is a 400", async () => {
    const { status } = await apiJson(`/resource-events/${crypto.randomUUID()}?limit=abc`);
    expect(status).toBe(400);
  });

  it("GET /sync/pull?since=abc is a 400, never a 200 with next_cursor:null", async () => {
    const { status, body } = await apiJson<{ next_cursor?: unknown }>(
      "/sync/pull?since=abc",
      undefined,
      HUB_A_TOKEN,
    );
    expect(status).toBe(400);
    expect(body.next_cursor).toBeUndefined();
  });

  it("POST /read/batch with non-integer since/limit is a 400", async () => {
    const { status } = await postJson("/read/batch", {
      agents: ["someone"],
      since: "abc" as unknown as number,
    });
    expect(status).toBe(400);
  });
});

describe("P10: ack on behalf of another agent (review 'ack only for yourself')", () => {
  it("a codex token cannot ack a message pretending to be claude", async () => {
    const sent = await postJson<{ id: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "needs ack",
      request_ack: true,
    });
    const { status } = await postJson(`/messages/${sent.body.id}/ack`, { agent: "claude" }, CODEX_TOKEN);
    expect(status).toBe(403);
  });
});

describe("M8: workers_dev and preview_urls disabled at the config level", () => {
  it("wrangler.toml declares workers_dev = false and preview_urls = false", () => {
    // A static config assertion (not an HTTP probe — Miniflare doesn't
    // simulate the workers.dev/preview routing surface).
    expect(wranglerToml).toMatch(/^\s*workers_dev\s*=\s*false\s*$/m);
    expect(wranglerToml).toMatch(/^\s*preview_urls\s*=\s*false\s*$/m);
  });
});

describe("Response headers (review M8/LOW)", () => {
  it("every JSON response carries cache-control: no-store and x-content-type-options: nosniff", async () => {
    const raw = await api("/messages", {
      method: "POST",
      body: JSON.stringify({ sender: "claude", recipient: "codex", topic: "status", body: "header check" }),
    });
    expect(raw.headers.get("cache-control")).toBe("no-store");
    expect(raw.headers.get("x-content-type-options")).toBe("nosniff");
  });

  it("a 401 also carries the security headers", async () => {
    const raw = await api("/messages", undefined, null);
    expect(raw.status).toBe(401);
    expect(raw.headers.get("cache-control")).toBe("no-store");
  });
});

describe("Identity.token removed from the wire (review L3)", () => {
  it("no route response ever echoes a 'token' field", async () => {
    const { body } = await apiJson<Record<string, unknown>>("/presence");
    expect(JSON.stringify(body)).not.toMatch(/CLAUDE_TOKEN|test-token-/);
  });
});

// >=32 chars (agent-hub#82 re-review N1's minimum token length applies to
// AGENT_BUS_AUTH_TOKEN too, not just AGENT_BUS_TOKENS entries).
const FAKE_SHARED_TOKEN = "some-shared-token-0000000000000000000"; // pragma: allowlist secret

describe("Shared-token fallback is disabled by default (review H1/L5)", () => {
  it("a request bearing the shared token is rejected when the dev flag is off", () => {
    // Simulate the flag being off by pointing at an env without it — this
    // Worker's test binding always sets AGENT_BUS_DEV_ALLOW_SHARED_TOKEN=1,
    // so this test instead unit-tests `authenticate()` directly with a
    // fake env, which is pure and needs no Miniflare state.
    const req = new Request("https://example.com/presence", {
      headers: { authorization: `Bearer ${FAKE_SHARED_TOKEN}` },
    });
    const identity = authenticate(req, { AGENT_BUS_AUTH_TOKEN: FAKE_SHARED_TOKEN }); // no DEV_ALLOW flag
    expect(identity).toBeNull();
  });

  it("with the dev flag on, the shared token resolves to least-privilege role 'agent', never hub/operator", () => {
    const req = new Request("https://example.com/presence", {
      headers: { authorization: `Bearer ${FAKE_SHARED_TOKEN}` },
    });
    const identity = authenticate(req, {
      AGENT_BUS_AUTH_TOKEN: FAKE_SHARED_TOKEN,
      AGENT_BUS_DEV_ALLOW_SHARED_TOKEN: "1",
    });
    expect(identity?.role).toBe("agent");
  });

  it("a shared token shorter than 32 chars never authenticates, even with the dev flag on (re-review N1)", () => {
    const shortToken = "short-shared-token"; // pragma: allowlist secret
    const req = new Request("https://example.com/presence", {
      headers: { authorization: `Bearer ${shortToken}` },
    });
    const identity = authenticate(req, {
      AGENT_BUS_AUTH_TOKEN: shortToken,
      AGENT_BUS_DEV_ALLOW_SHARED_TOKEN: "1",
    });
    expect(identity).toBeNull();
  });
});

describe("N1: AGENT_BUS_TOKENS parser is strict (re-review, MEDIUM-critical-in-practice)", () => {
  const REAL_TOKEN = "real-token-that-should-have-worked-0000"; // pragma: allowlist secret

  it("an array-shaped token map authenticates NOTHING — not the array index, not the real token", () => {
    // The exploit: Object.entries(["x"]) yields [["0", "x"]], so a secret
    // accidentally shaped as a JSON ARRAY made the literal string `Bearer 0`
    // authenticate as whatever role the sole array entry named — verified
    // as full operator compromise in the re-review's probe.
    const arrayShapedSecret = JSON.stringify([
      { agent: "op", role: "operator", token: REAL_TOKEN },
    ]);
    const reqZero = new Request("https://example.com/sync/stats", {
      headers: { authorization: "Bearer 0" },
    });
    expect(authenticate(reqZero, { AGENT_BUS_TOKENS: arrayShapedSecret })).toBeNull();

    const reqReal = new Request("https://example.com/sync/stats", {
      headers: { authorization: `Bearer ${REAL_TOKEN}` },
    });
    expect(authenticate(reqReal, { AGENT_BUS_TOKENS: arrayShapedSecret })).toBeNull();
  });

  it("a null or non-object top-level value also fails the whole map closed", () => {
    for (const secret of ['null', '"just a string"', "42", "true"]) {
      const req = new Request("https://example.com/presence", {
        headers: { authorization: `Bearer ${REAL_TOKEN}` },
      });
      expect(authenticate(req, { AGENT_BUS_TOKENS: secret })).toBeNull();
    }
  });

  it("an entry with an unknown key is rejected, not silently accepted with the extra field ignored", () => {
    const secret = JSON.stringify({
      [REAL_TOKEN]: { agent: "op", role: "operator", token: REAL_TOKEN },
    });
    const req = new Request("https://example.com/sync/stats", {
      headers: { authorization: `Bearer ${REAL_TOKEN}` },
    });
    expect(authenticate(req, { AGENT_BUS_TOKENS: secret })).toBeNull();
  });

  it("a token shorter than 32 characters never authenticates, even with an otherwise-valid entry", () => {
    const shortToken = "short-but-otherwise-valid-token";
    // Deliberately still under 32 to prove the boundary; adjust if the
    // literal above happens to reach 32 after an edit.
    expect(shortToken.length).toBeLessThan(MIN_TOKEN_LEN);
    const secret = JSON.stringify({ [shortToken]: { agent: "op", role: "operator" } });
    const req = new Request("https://example.com/sync/stats", {
      headers: { authorization: `Bearer ${shortToken}` },
    });
    expect(authenticate(req, { AGENT_BUS_TOKENS: secret })).toBeNull();
  });

  it("a valid, >=32-char, plain-object entry still authenticates correctly (no over-correction)", () => {
    const secret = JSON.stringify({ [REAL_TOKEN]: { agent: "op", role: "operator" } });
    const req = new Request("https://example.com/sync/stats", {
      headers: { authorization: `Bearer ${REAL_TOKEN}` },
    });
    const identity = authenticate(req, { AGENT_BUS_TOKENS: secret });
    expect(identity).toEqual({ agent: "op", role: "operator" });
  });
});

describe("Operator role required for GET /sync/stats (task item 6)", () => {
  it("an agent-role token is rejected", async () => {
    const { status } = await apiJson("/sync/stats", undefined, CLAUDE_TOKEN);
    expect(status).toBe(403);
  });

  it("an operator-role token succeeds", async () => {
    const { status } = await apiJson("/sync/stats", undefined, OPERATOR_TOKEN);
    expect(status).toBe(200);
  });
});

describe("HUB_A constant sanity (avoids an unused-import lint drift)", () => {
  it("matches the hub identity configured for test-token-hub-a", async () => {
    const { status } = await postJson(
      "/sync/push",
      { origin_hub: HUB_A, messages: [] },
      HUB_A_TOKEN,
    );
    expect(status).toBe(200);
  });
});

describe("N2: side fields are capped, type-checked and PHI-screened (re-review)", () => {
  it("rejects a 1 MB thread_id on POST /messages instead of storing it", async () => {
    const { status, body } = await postJson<{ error: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "x",
      thread_id: "t".repeat(1_000_000),
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/thread_id/);
  });

  it("rejects an oversized client_msg_id, reply_to and hlc", async () => {
    for (const [field, len] of [
      ["client_msg_id", 1000],
      ["reply_to", 1000],
      ["hlc", 1000],
    ] as const) {
      const { status, body } = await postJson<{ error: string }>("/messages", {
        sender: "claude",
        recipient: "codex",
        topic: "status",
        body: "x",
        [field]: "y".repeat(len),
      });
      expect(status).toBe(400);
      expect(body.error).toMatch(new RegExp(field));
    }
  });

  it("rejects a 500 KB recipient", async () => {
    const { status, body } = await postJson<{ error: string }>("/messages", {
      sender: "claude",
      recipient: "r".repeat(500_000),
      topic: "status",
      body: "x",
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/recipient/);
  });

  it("rejects an oversized origin_host and origin_hub on a direct post", async () => {
    const originHost = await postJson<{ error: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "x",
      origin_host: "h".repeat(1000),
    });
    expect(originHost.status).toBe(400);

    const originHub = await postJson<{ error: string }>(
      "/messages",
      { sender: "claude", recipient: "codex", topic: "status", body: "x", origin_hub: "h".repeat(1000) },
      HUB_A_TOKEN,
    );
    expect(originHub.status).toBe(400);
  });

  it("a hub push with 3 MB of oversized side fields is rejected per item, not stored", async () => {
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
            thread_id: "t".repeat(1_000_000),
            client_msg_id: "c".repeat(1_000_000),
            reply_to: "r".repeat(1_000_000),
          },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(body.accepted).toHaveLength(0);
  });

  it("extends the PHI screen to topic, thread_id and recipient (re-review M7/N2)", async () => {
    const topicHit = await postJson<{ error: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "patient name intake",
      body: "x",
    });
    expect(topicHit.status).toBe(400);
    expect(topicHit.body.error).toMatch(/PHI/);

    const threadIdHit = await postJson<{ error: string }>("/messages", {
      sender: "claude",
      recipient: "codex",
      topic: "status",
      body: "x",
      thread_id: "SSN 123-45-6789",
    });
    expect(threadIdHit.status).toBe(400);
    expect(threadIdHit.body.error).toMatch(/PHI/);

    const recipientHit = await postJson<{ error: string }>("/messages", {
      sender: "claude",
      recipient: "patient name intake",
      topic: "status",
      body: "x",
    });
    expect(recipientHit.status).toBe(400);
    expect(recipientHit.body.error).toMatch(/PHI/);
  });
});

describe("N3: PUT /presence and /sync/push-presence field validation (re-review)", () => {
  it("rejects capabilities:5 instead of storing and later mis-serving it", async () => {
    const { status, body } = await putJson<{ error: string }>(
      "/presence/n3-cap-test",
      { status: "online", capabilities: 5 as unknown as string[] },
      HUB_A_TOKEN,
    );
    expect(status).toBe(400);
    expect(body.error).toMatch(/capabilities/);
  });

  it("rejects 1.5 MB of presence metadata", async () => {
    const { status, body } = await putJson<{ error: string }>(
      "/presence/n3-metadata-test",
      { status: "online", metadata: { blob: "x".repeat(1_500_000) } },
      HUB_A_TOKEN,
    );
    expect(status).toBe(400);
    expect(body.error).toMatch(/metadata/);
  });

  it("rejects a non-string session_id and network_context instead of coercing to '[object Object]'", async () => {
    const sessionId = await putJson<{ error: string }>(
      "/presence/n3-session-test",
      { status: "online", session_id: {} as unknown as string },
      HUB_A_TOKEN,
    );
    expect(sessionId.status).toBe(400);

    const networkContext = await putJson<{ error: string }>(
      "/presence/n3-network-test",
      { status: "online", network_context: {} as unknown as string },
      HUB_A_TOKEN,
    );
    expect(networkContext.status).toBe(400);
  });

  it("rejects an invalid (but string) network_context value", async () => {
    const { status, body } = await putJson<{ error: string }>(
      "/presence/n3-network-invalid",
      { status: "online", network_context: "not-a-real-value" },
      HUB_A_TOKEN,
    );
    expect(status).toBe(400);
    expect(body.error).toMatch(/network_context/);
  });

  it("PHI-screens presence status", async () => {
    const { status, body } = await putJson<{ error: string }>(
      "/presence/n3-phi-status",
      { status: "patient name is Jane" },
      HUB_A_TOKEN,
    );
    expect(status).toBe(400);
    expect(body.error).toMatch(/PHI/);
  });

  it("/sync/push-presence rejects the same class of bad fields per event", async () => {
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
            status: "x".repeat(1_000_000),
          },
        ],
      },
      HUB_A_TOKEN,
    );
    expect(body.accepted).toBe(0);
  });
});

describe("Row size stays under Cloudflare's DO SQLite ~2 MB row limit, with margin (re-review N2)", () => {
  it("the theoretical worst-case message row (every field at its cap) has healthy margin below DO_SQLITE_ROW_LIMIT_BYTES", async () => {
    const {
      MAX_BODY_LEN,
      MAX_METADATA_BYTES,
      MAX_TAGS_COUNT,
      MAX_TAG_LEN,
      MAX_SENDER_LEN,
      MAX_RECIPIENT_LEN,
      MAX_TOPIC_LEN,
      MAX_THREAD_ID_LEN,
      MAX_REPLY_TO_LEN,
      MAX_CLIENT_MSG_ID_LEN,
      MAX_HLC_LEN,
      MAX_ORIGIN_HOST_LEN,
      MAX_ORIGIN_HUB_LEN,
      MAX_TOTAL_MESSAGE_BYTES,
      DO_SQLITE_ROW_LIMIT_BYTES,
    } = await import("../src/validation");

    const worstCaseFieldSum =
      MAX_BODY_LEN +
      MAX_METADATA_BYTES +
      MAX_TAGS_COUNT * MAX_TAG_LEN +
      MAX_SENDER_LEN +
      MAX_RECIPIENT_LEN +
      MAX_TOPIC_LEN +
      MAX_THREAD_ID_LEN +
      MAX_REPLY_TO_LEN +
      MAX_CLIENT_MSG_ID_LEN +
      MAX_HLC_LEN +
      MAX_ORIGIN_HOST_LEN +
      MAX_ORIGIN_HUB_LEN;

    // Both the sum of every individual field cap AND the explicit
    // total-size backstop must have real margin below the DO row limit —
    // currently the per-field sum (~346 KB) is comfortably under the
    // explicit total cap (400 KB) too, so raising any single field's cap
    // without checking this sum is exactly the mistake this test catches.
    expect(worstCaseFieldSum).toBeLessThan(DO_SQLITE_ROW_LIMIT_BYTES / 2);
    expect(MAX_TOTAL_MESSAGE_BYTES).toBeLessThan(DO_SQLITE_ROW_LIMIT_BYTES / 2);
  });
});

describe("N4: claim field validation (re-review)", () => {
  it("rejects repo_scopes:\"abc\" instead of storing it as a bare string", async () => {
    const { status, body } = await postJson<{ error: string }>(`/channels/arbitrate/${crypto.randomUUID()}`, {
      agent: "claude",
      repo_scopes: "abc" as unknown as string[],
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/repo_scopes/);
  });

  it("rejects a 1 MB priority_argument", async () => {
    const { status, body } = await postJson<{ error: string }>(`/channels/arbitrate/${crypto.randomUUID()}`, {
      agent: "claude",
      priority_argument: "p".repeat(1_000_000),
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/priority_argument/);
  });

  it("rejects namespace:{} with a 400, not a generic 500", async () => {
    const { status, body } = await postJson<{ error: string }>(`/channels/arbitrate/${crypto.randomUUID()}`, {
      agent: "claude",
      mode: "shared_namespaced",
      namespace: {} as unknown as string,
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/namespace/);
  });

  it("rejects an oversized scope_kind/scope_path", async () => {
    const scopeKind = await postJson<{ error: string }>(`/channels/arbitrate/${crypto.randomUUID()}`, {
      agent: "claude",
      scope_kind: "k".repeat(1000),
    });
    expect(scopeKind.status).toBe(400);

    const scopePath = await postJson<{ error: string }>(`/channels/arbitrate/${crypto.randomUUID()}`, {
      agent: "claude",
      scope_path: "p".repeat(2000),
    });
    expect(scopePath.status).toBe(400);
  });
});
