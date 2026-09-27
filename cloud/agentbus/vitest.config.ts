import { cloudflareTest } from "@cloudflare/vitest-plugin";
import { defineConfig } from "vitest/config";

export default defineConfig({
  plugins: [
    cloudflareTest({
      wrangler: { configPath: "./wrangler.toml" },
      // Test-only bindings, never used at deploy time. Real deploys set
      // AGENT_BUS_TOKENS / AGENT_BUS_AUTH_TOKEN via `wrangler secret put`
      // (see README.md); these values only exist inside the Miniflare
      // sandbox this config spins up for `vitest run`.
      //
      // agent-hub#82: token-map entries now carry a `role` ("agent" | "hub" |
      // "operator") and, for hub-role entries, a `hub` identity — see
      // `src/auth.ts`. Two distinct hub tokens (hubA/hubB) exist so tests can
      // exercise cross-origin behavior (H5's `origin_hub` spoofing fix, the
      // `(origin_hub, id)` composite dedup key). `AGENT_BUS_AUTH_TOKEN` is
      // gated behind `AGENT_BUS_DEV_ALLOW_SHARED_TOKEN` and used only by
      // `health-auth.test.ts` to exercise the dev-fallback path itself.
      miniflare: {
        bindings: {
          AGENT_BUS_AUTH_TOKEN: "test-shared-token",
          AGENT_BUS_DEV_ALLOW_SHARED_TOKEN: "1",
          AGENT_BUS_TOKENS: JSON.stringify({
            "test-token-claude": { agent: "claude", host: "test-host", role: "agent" },
            "test-token-codex": { agent: "codex", host: "test-host", role: "agent" },
            "test-token-hub-a": { agent: "hub-a-relay", role: "hub", hub: "asuspro13" },
            "test-token-hub-b": { agent: "hub-b-relay", role: "hub", hub: "spark-0060" },
            "test-token-operator": { agent: "operator", role: "operator" },
          }),
        },
      },
    }),
  ],
});
