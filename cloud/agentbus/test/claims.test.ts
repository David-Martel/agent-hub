import { describe, expect, it } from "vitest";
import { api, apiJson, CODEX_TOKEN, OPERATOR_TOKEN, postJson, putJson } from "./helpers";

function resource(name: string): string {
  // Every test gets its own resource name so ClaimDO instances (one per
  // resource, via idFromName) never leak state between tests.
  return `${name}-${crypto.randomUUID()}`;
}

// agent-hub#82 security review (H2/P2): claim/renew/release are now
// owner-only for agent-role tokens — the bearer token's own identity must
// match the `agent` field. Every "codex" actor below authenticates with
// CODEX_TOKEN (not the default CLAUDE_TOKEN) for exactly this reason.

describe("POST /channels/arbitrate/:resource (claim)", () => {
  it("grants a lone exclusive claim", async () => {
    const res = resource("solo");
    const { status, body } = await postJson<{ status: string; agent: string; mode: string }>(
      `/channels/arbitrate/${res}`,
      { agent: "claude", priority_argument: "first-edit required" },
    );
    expect(status).toBe(200);
    expect(body.status).toBe("granted");
    expect(body.mode).toBe("exclusive");
  });

  it("contests a second exclusive claim and attaches a reroute_suggestion", async () => {
    const res = resource("contest");
    await postJson(`/channels/arbitrate/${res}`, { agent: "claude" });
    const { body } = await postJson<{
      status: string;
      reroute_suggestion?: { original_resource: string; reason: string };
    }>(`/channels/arbitrate/${res}`, { agent: "codex" }, CODEX_TOKEN);
    expect(body.status).toBe("contested");
    expect(body.reroute_suggestion?.original_resource).toBe(res);
    expect(body.reroute_suggestion?.reason).toContain("claude");
  });

  it("does not conflict for two shared claims", async () => {
    const res = resource("shared");
    await postJson(`/channels/arbitrate/${res}`, { agent: "claude", mode: "shared" });
    const { body } = await postJson<{ status: string }>(
      `/channels/arbitrate/${res}`,
      { agent: "codex", mode: "shared" },
      CODEX_TOKEN,
    );
    expect(body.status).toBe("granted");
  });

  it("known machine-global resources (cargo-target) suggest a namespaced reroute", async () => {
    const res = `cargo-target-${crypto.randomUUID()}`;
    await postJson(`/channels/arbitrate/${res}`, { agent: "claude" });
    const { body } = await postJson<{ reroute_suggestion?: { suggested_resource: string } }>(
      `/channels/arbitrate/${res}`,
      { agent: "codex" },
      CODEX_TOKEN,
    );
    // REROUTE_RULES' suggested_resource template is the fixed pattern name
    // ("cargo-target:{agent}"), not the full matched resource string — this
    // mirrors `suggest_reroute` in `agent-bus-core::channels` exactly.
    expect(body.reroute_suggestion?.suggested_resource).toBe("cargo-target:codex");
  });

  it("rejects shared_namespaced without a namespace", async () => {
    const res = resource("ns-missing");
    const { status, body } = await postJson<{ error: string }>(`/channels/arbitrate/${res}`, {
      agent: "claude",
      mode: "shared_namespaced",
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/namespace/);
  });

  it("EXCLUSIVE CLAIM CONTENTION: two concurrent claims on the same resource — the DO serializes them", async () => {
    const res = resource("concurrent");
    const [a, b] = await Promise.all([
      postJson<{ status: string; agent: string }>(`/channels/arbitrate/${res}`, { agent: "claude" }),
      postJson<{ status: string; agent: string }>(`/channels/arbitrate/${res}`, { agent: "codex" }, CODEX_TOKEN),
    ]);
    const statuses = [a.body.status, b.body.status].sort();
    // The DO serializes the two requests, so exactly one of them is
    // processed while it is still the ONLY claim on the resource (and gets
    // "granted" in its own response) — the other is processed once both
    // claims exist and gets "contested". This is the faithful mirror of
    // `claim_resource_with_options`: it returns the status computed at the
    // moment of THAT call, not a retroactively-updated view.
    expect(statuses).toEqual(["contested", "granted"]);

    // But the STORED state reflects `recompute_claim_statuses` over the full
    // set as of the last write: once both claims exist, persisting either one
    // recomputes ALL claims, so both end up "contested" in the arbitration
    // state a caller reads afterward — including the one that was originally
    // told "granted".
    const state = await apiJson<{ claims: Array<{ agent: string; status: string }> }>(
      `/channels/arbitrate/${res}`,
    );
    expect(state.body.claims).toHaveLength(2);
    expect(state.body.claims.every((c) => c.status === "contested")).toBe(true);
  });
});

describe("GET /channels/arbitrate/:resource (arbitration state)", () => {
  it("returns an empty ArbitrationState for a never-claimed resource", async () => {
    const res = resource("never-claimed");
    const { status, body } = await apiJson<{ resource: string; claims: unknown[]; winner: null }>(
      `/channels/arbitrate/${res}`,
    );
    expect(status).toBe(200);
    expect(body).toEqual({ resource: res, claims: [], winner: null, resolution_reason: null });
  });
});

describe("PUT /channels/arbitrate/:resource/resolve", () => {
  it("names a winner: granted for the winner, review_assigned for the rest (operator role required)", async () => {
    const res = resource("resolve");
    await postJson(`/channels/arbitrate/${res}`, { agent: "claude" });
    await postJson(`/channels/arbitrate/${res}`, { agent: "codex" }, CODEX_TOKEN);

    const { status, body } = await putJson<{
      winner: string;
      resolution_reason: string;
      claims: Array<{ agent: string; status: string }>;
    }>(
      `/channels/arbitrate/${res}/resolve`,
      { winner: "claude", reason: "higher priority argument" },
      OPERATOR_TOKEN,
    );

    expect(status).toBe(200);
    expect(body.winner).toBe("claude");
    expect(body.resolution_reason).toBe("higher priority argument");
    const claude = body.claims.find((c) => c.agent === "claude");
    const codex = body.claims.find((c) => c.agent === "codex");
    expect(claude?.status).toBe("granted");
    expect(codex?.status).toBe("review_assigned");
  });

  it("rejects resolving a resource with no claims (400, still requires operator role)", async () => {
    const res = resource("resolve-empty");
    const { status, body } = await putJson<{ error: string }>(
      `/channels/arbitrate/${res}/resolve`,
      { winner: "claude" },
      OPERATOR_TOKEN,
    );
    expect(status).toBe(400);
    expect(body.error).toMatch(/no claims found/);
  });

  it("REJECTS a non-operator token (403) — resolve was previously open to every token (review H2)", async () => {
    const res = resource("resolve-forbidden");
    await postJson(`/channels/arbitrate/${res}`, { agent: "claude" });
    const { status } = await putJson(`/channels/arbitrate/${res}/resolve`, { winner: "claude" }); // default CLAUDE_TOKEN, role "agent"
    expect(status).toBe(403);
  });
});

describe("POST /channels/arbitrate/:resource/renew", () => {
  it("extends the lease and keeps the claim granted", async () => {
    const res = resource("renew");
    await postJson(`/channels/arbitrate/${res}`, { agent: "claude", lease_ttl_seconds: 3600 });
    const { status, body } = await postJson<{ status: string; lease_ttl_seconds: number }>(
      `/channels/arbitrate/${res}/renew`,
      { agent: "claude", lease_ttl_seconds: 7200 },
    );
    expect(status).toBe(200);
    expect(body.status).toBe("granted");
    expect(body.lease_ttl_seconds).toBe(7200);
  });

  it("renewing your own nonexistent claim mirrors the Rust hub's 500 (AgentBusError::Internal, not 400)", async () => {
    const res = resource("renew-missing");
    // Renewing as "claude" (matching the bearer token's own identity) on a
    // resource claude has never claimed — owner-only binding lets this
    // request through, then the DO's own missing-claim check 500s.
    const { status } = await postJson(`/channels/arbitrate/${res}/renew`, { agent: "claude" });
    expect(status).toBe(500);
  });

  it("REJECTS renewing as a different agent (403, owner-only — review H2/P2)", async () => {
    const res = resource("renew-forbidden");
    await postJson(`/channels/arbitrate/${res}`, { agent: "codex" }, CODEX_TOKEN);
    // Authenticated as claude, but trying to renew codex's claim.
    const { status } = await postJson(`/channels/arbitrate/${res}/renew`, { agent: "codex" });
    expect(status).toBe(403);
  });
});

describe("POST /channels/arbitrate/:resource/release", () => {
  it("releases a claim and the resource becomes claimable again", async () => {
    const res = resource("release");
    await postJson(`/channels/arbitrate/${res}`, { agent: "claude" });
    const { status, body } = await postJson<{ claims: unknown[] }>(`/channels/arbitrate/${res}/release`, {
      agent: "claude",
    });
    expect(status).toBe(200);
    expect(body.claims).toEqual([]);

    const reclaim = await postJson<{ status: string }>(`/channels/arbitrate/${res}`, { agent: "codex" }, CODEX_TOKEN);
    expect(reclaim.body.status).toBe("granted");
  });

  it("releasing your own nonexistent claim is a 400", async () => {
    const res = resource("release-missing");
    const { status, body } = await postJson<{ error: string }>(`/channels/arbitrate/${res}/release`, {
      agent: "claude",
    });
    expect(status).toBe(400);
    expect(body.error).toMatch(/no active claim/);
  });

  it("REJECTS releasing another agent's claim (403, owner-only — review H2/P2)", async () => {
    const res = resource("release-forbidden");
    await postJson(`/channels/arbitrate/${res}`, { agent: "codex" }, CODEX_TOKEN);
    const { status } = await postJson(`/channels/arbitrate/${res}/release`, { agent: "codex" }); // default CLAUDE_TOKEN
    expect(status).toBe(403);
  });
});

describe("lease expiry", () => {
  it("a 1-second lease is pruned from the arbitration state shortly after it expires", async () => {
    const res = resource("expiry");
    await postJson(`/channels/arbitrate/${res}`, { agent: "claude", lease_ttl_seconds: 1 });
    const before = await apiJson<{ claims: unknown[] }>(`/channels/arbitrate/${res}`);
    expect(before.body.claims).toHaveLength(1);

    await new Promise((resolveDelay) => setTimeout(resolveDelay, 1200));

    const after = await apiJson<{ claims: unknown[] }>(`/channels/arbitrate/${res}`);
    expect(after.body.claims).toEqual([]);
  }, 10_000);
});

describe("claim/renew lease TTL cap (review M4)", () => {
  it("caps an oversized lease_ttl_seconds instead of overflowing the expiry timestamp", async () => {
    const res = resource("ttl-cap");
    const { status, body } = await postJson<{ lease_ttl_seconds: number }>(`/channels/arbitrate/${res}`, {
      agent: "claude",
      lease_ttl_seconds: 1e13,
    });
    expect(status).toBe(200);
    expect(body.lease_ttl_seconds).toBeLessThanOrEqual(86_400);
  });

  it("rejects a non-numeric lease_ttl_seconds with 400, not a 500 'Invalid time value'", async () => {
    const res = resource("ttl-bad");
    const { status, body } = await postJson<{ error: string }>(`/channels/arbitrate/${res}`, {
      agent: "claude",
      lease_ttl_seconds: "abc" as unknown as number,
    });
    expect(status).toBe(400);
    expect(body.error).not.toMatch(/Invalid time value/);
  });
});

describe("GET /resource-events/:resource_id", () => {
  it("records claimed/contested/resolved/released lifecycle events", async () => {
    const res = resource("events");
    await postJson(`/channels/arbitrate/${res}`, { agent: "claude" });
    await postJson(`/channels/arbitrate/${res}`, { agent: "codex" }, CODEX_TOKEN);
    await putJson(`/channels/arbitrate/${res}/resolve`, { winner: "claude" }, OPERATOR_TOKEN);
    await postJson(`/channels/arbitrate/${res}/release`, { agent: "codex" }, CODEX_TOKEN);

    const { status, body } = await apiJson<Array<{ event: string }>>(`/resource-events/${res}`);
    expect(status).toBe(200);
    const events = body.map((e) => e.event);
    expect(events).toContain("claimed");
    expect(events).toContain("contested");
    expect(events).toContain("resolved");
    expect(events).toContain("released");
  });

  it("does not mint a ClaimDO for a resource nobody has ever claimed (review M6)", async () => {
    const res = resource("events-unclaimed");
    const { status, body } = await apiJson<unknown[]>(`/resource-events/${res}`);
    expect(status).toBe(200);
    expect(body).toEqual([]);
  });
});

describe("resource name length cap (review M6, must 400 not 500)", () => {
  it("an oversized resource name is a JSON 400 with security headers, not Hono's default plain-text 500", async () => {
    const res = await api(
      `/channels/arbitrate/${"x".repeat(300)}`,
      { method: "POST", body: JSON.stringify({ agent: "claude" }) },
    );
    expect(res.status).toBe(400);
    expect(res.headers.get("content-type")).toMatch(/application\/json/);
    expect(res.headers.get("cache-control")).toBe("no-store");
    const body = (await res.json()) as { error: string };
    expect(body.error).toMatch(/resource/);
  });
});

describe("resource name normalization (review M9)", () => {
  it("Foo.rs and foo.rs share the same ClaimDO shard", async () => {
    const base = `Case-Fold-${crypto.randomUUID()}`;
    await postJson(`/channels/arbitrate/${base}`, { agent: "claude" });
    const { body } = await apiJson<{ claims: Array<{ agent: string }> }>(
      `/channels/arbitrate/${base.toLowerCase()}`,
    );
    expect(body.claims.some((c) => c.agent === "claude")).toBe(true);
  });

  it("a\\b and a/b share the same ClaimDO shard", async () => {
    const unique = crypto.randomUUID();
    await postJson(`/channels/arbitrate/${encodeURIComponent(`a\\${unique}\\b`)}`, { agent: "claude" });
    const { body } = await apiJson<{ claims: Array<{ agent: string }> }>(
      `/channels/arbitrate/${encodeURIComponent(`a/${unique}/b`)}`,
    );
    expect(body.claims.some((c) => c.agent === "claude")).toBe(true);
  });
});

describe("auth on claims routes", () => {
  it("requires a bearer token", async () => {
    const res = await api(`/channels/arbitrate/${resource("noauth")}`, undefined, null);
    expect(res.status).toBe(401);
  });
});
