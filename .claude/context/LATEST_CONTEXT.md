# Latest Context Pointer

**Latest context:** [`agent-bus-context-2026-06-13.md`](./agent-bus-context-2026-06-13.md)

- **ID:** `ctx-agent-bus-20260613`
- **Created:** 2026-06-13
- **Branch / commit:** `main` @ `a151397`
- **Summary:** Audited TODO trackers against real code. Found the `rust-cli`
  crate already removed (workspace split done; `cargo check` clean) but CI,
  `release.yml`, `build.ps1`, and `sgconfig.yml` still reference it (broken).
  Updated TODO.md (added P8 post-split remediation + P9 robustness) and
  agents.TODO.md (refreshed checkpoint/test inventory). Next: land CI/build
  fixes + in-flight migration, then deploy parallel fleet.

Index: [`CONTEXT_INDEX.json`](./CONTEXT_INDEX.json)
