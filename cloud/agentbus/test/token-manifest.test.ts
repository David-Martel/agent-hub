import { env } from "cloudflare:workers";
import { describe, expect, it } from "vitest";
import app from "../src/index";
import { tokenManifest } from "../src/token-manifest";
import { api, CLAUDE_TOKEN, HUB_A_TOKEN, OPERATOR_TOKEN } from "./helpers";

describe("operator-only exact raw token binding provenance", () => {
  it("returns a full-binding digest/count with no entries, roles or credentials", async () => {
    const response = await api("/admin/tokens/manifest", undefined, OPERATOR_TOKEN);
    expect(response.status).toBe(200);
    expect(response.headers.get("cache-control")).toBe("no-store");
    expect(response.headers.get("x-content-type-options")).toBe("nosniff");
    const text = await response.text();
    const manifest = JSON.parse(text);
    expect(Object.keys(manifest).sort()).toEqual(["build_version", "entry_count", "hub_identity", "representation", "sha256"]);
    expect(manifest).toMatchObject({ representation: "utf8-secret-binding-v1", build_version: "agentbus-cloud@0.2.0", hub_identity: "cloud",
      entry_count: Object.keys(JSON.parse(env.AGENT_BUS_TOKENS!)).length });
    expect(manifest.sha256).toMatch(/^[a-f0-9]{64}$/);
    expect(text).not.toContain(OPERATOR_TOKEN);
    expect(text).not.toContain(CLAUDE_TOKEN);
    expect(text).not.toContain('"role"');
  });

  it.each([CLAUDE_TOKEN, HUB_A_TOKEN])("denies agent and hub roles (%s)", async (token) => {
    const response = await api("/admin/tokens/manifest", undefined, token);
    expect(response.status).toBe(403);
    expect(response.headers.get("cache-control")).toBe("no-store");
  });

  it("does not authenticate absent or invalid tokens", async () => {
    expect((await api("/admin/tokens/manifest", undefined, null)).status).toBe(401);
    expect((await api("/admin/tokens/manifest", undefined, "invalid")).status).toBe(401);
  });

  it("hashes all exact UTF-8 bytes including whitespace and entries auth would filter out", async () => {
    // Fixed known digest independently obtained with Python hashlib.sha256,
    // not by calling the implementation's digest calculation in the assertion.
    const raw = '{ "unusable": {"unknown": "雪"}, "other": null }\n';
    expect(await tokenManifest(raw)).toEqual({ representation: "utf8-secret-binding-v1",
      sha256: "4dee97039ccdfce48e32dd6791a4113567b39b6a820a6bf0865a2bba41010557", entry_count: 2 });
    expect((await tokenManifest(raw.trim())).sha256).not.toBe((await tokenManifest(raw)).sha256);
    const liveFixture = `{\n "${OPERATOR_TOKEN}": {"agent":"operator","role":"operator"},\n "unusable": {"unknown":"雪"}\n}\n`;
    const response = await app.request("/admin/tokens/manifest", { headers: { authorization: `Bearer ${OPERATOR_TOKEN}` } }, { ...env, AGENT_BUS_TOKENS: liveFixture });
    expect(response.status).toBe(200);
    expect(await response.json()).toMatchObject({ entry_count: 2,
      sha256: "705fafc0dfcf0ceebd0181b36431428f0bd67ebc12ef9646ef747e81740ff800" });
    expect((await tokenManifest(liveFixture)).sha256).not.toBe((await tokenManifest(JSON.stringify(JSON.parse(liveFixture)))).sha256);
  });

  it.each([undefined, "not-json", "null", "[]", "42", '"string"'])("refuses malformed/non-object binding without payload details (%s)", async (raw) => {
    await expect(tokenManifest(raw)).rejects.toThrow(/^token manifest unavailable$/);
  });

  it("fails closed at the real route when the token-map binding is malformed", async () => {
    const response = await app.request("/admin/tokens/manifest", { headers: { authorization: `Bearer ${OPERATOR_TOKEN}` } }, { ...env, AGENT_BUS_TOKENS: "not-json" });
    expect(response.status).toBe(401);
    expect(await response.text()).not.toContain("not-json");
  });
});
