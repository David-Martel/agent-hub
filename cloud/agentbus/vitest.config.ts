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
      //
      // agent-hub#82 re-review N1: every token below is >=32 characters —
      // the parser now rejects a shorter token-map key outright (see
      // `MIN_TOKEN_LEN` in `src/auth.ts`). Keep in sync with the literal
      // values in `test/helpers.ts`.
      miniflare: {
        bindings: {
          AGENT_BUS_AUTH_TOKEN: "test-shared-token-0123456789012345678901",
          AGENT_BUS_DEV_ALLOW_SHARED_TOKEN: "1",
          AGENT_BUS_TOKENS: JSON.stringify({
            "test-token-claude-0123456789012345678901": { agent: "claude", host: "test-host", role: "agent" },
            "test-token-codex-01234567890123456789012": { agent: "codex", host: "test-host", role: "agent" },
            "test-token-hub-a-01234567890123456789012": { agent: "hub-a-relay", role: "hub", hub: "asuspro13" },
            "test-token-hub-b-01234567890123456789012": { agent: "hub-b-relay", role: "hub", hub: "spark-0060" },
            "test-token-operator-01234567890123456789": { agent: "operator", role: "operator" },
            // re-review N7: an agent-role token with NO `host` field, to
            // exercise bindOriginHost's fix (a host-less agent-role token
            // must not be able to self-assert origin_host).
            "test-token-nohost-012345678901234567890": { agent: "roaming-agent", role: "agent" },
          }),
        },
      },
    }),
  ],
});
