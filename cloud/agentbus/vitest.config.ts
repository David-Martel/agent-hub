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
      miniflare: {
        bindings: {
          AGENT_BUS_AUTH_TOKEN: "test-shared-token",
          AGENT_BUS_TOKENS: JSON.stringify({
            "test-token-claude": { agent: "claude", host: "test-host" },
            "test-token-codex": { agent: "codex", host: "test-host" },
          }),
        },
      },
    }),
  ],
});
