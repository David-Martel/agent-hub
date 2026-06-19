# Latest Context Pointer

**Latest context:** [`agent-bus-context-2026-06-16.md`](./agent-bus-context-2026-06-16.md)

- **ID:** `ctx-agent-bus-20260616`
- **Created:** 2026-06-16
- **Branch / commit / tag:** `main` @ `e3d2d95` (tag `v0.5.0`)
- **Summary:** Multi-session sweep. Merged PRs #13–#22 (dep bumps incl. breaking
  redis 1.x/thiserror 2/reqwest 0.13, authed integration tests, batch-send
  server-mode, cross-machine health script, AgentHub `-AllowRemote`, doctest
  fixes, git-version `build.rs`). Cut first tag **v0.5.0** + GitHub Release;
  `agent-bus --version` now embeds git metadata. Vigil-fleet: **full
  bidirectional SSH mesh** (fleet key `~/.ssh/id_ed25519`), **single shared bus
  hub on asuspro13:8400** with ALL nodes incl. dtm joined, versioned binaries
  deployed to dtm + both sparks (built on spark-3066 aarch64). Fixed CargoTools
  `cargo-route.ps1` exit-code crash. Deferred: asuspro13 (x86_64) version rebuild;
  spark-3066 sccache broken (bypassed).

Index: [`CONTEXT_INDEX.json`](./CONTEXT_INDEX.json)
