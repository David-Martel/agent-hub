# Agent Bus — Interop and Serialization Notes

Date: 2026-03-14

## Current contract (implemented)
- Wire format: JSON messages with deterministic key ordering.
- Schema stability: `protocol_version = 1.0`.
- Machine-friendly defaults: CLI commands default to `--encoding compact`.
- Runtime visibility: `BusHealth.runtime_metadata` includes process/runtime and optional codec metadata.

## Interop comparison

| Format / Protocol | Fit for agent bus | Tradeoffs | Fit level |
|---|---|---|---|
| Current JSON (+ compact) | Great as baseline: human-readable, easy to debug, quick to adopt in PowerShell and MCP contexts | Higher token volume than TOON/protobuf at peak; no schema IDs beyond versioning | Keep as default |
| A2A (Agent2Agent task-card model) | Good for framework-level handoff workflows and lifecycle metadata | Heavy mapping layer required between `agent-bus` messages and task cards; higher coupling to external adapters | Add adapter bridges only where needed |
| TOON | Useful for operator-visible, compact text snapshots and diff-like logs | Ecosystem/tooling is still specialized; no strong MCP transport standardization yet | Evaluate as optional log/console format |
| protobuf | Strong payload compactness and schema enforcement for internal transport and storage | Requires schema registry/versioning discipline and codec codegen in Python↔Rust/JS stacks | Add after protocol and ownership semantics are finalized |

## Current runtime

- The repository is now Rust-only. Deprecated Python runtime code and the PyO3 codec path have been removed.
- JSON remains the canonical wire format for interoperability and debugging.
- Compact/minimal/TOON encodings remain the primary token-efficiency surfaces for agent consumers.
- MessagePack and LZ4 remain optional transport/storage optimizations, not the default protocol contract.

## Architecture checkpoint (2026-04-03)

- The repo now has a workspace with `rust-cli`, `agent-bus-core`,
  `agent-bus-cli`, `agent-bus-http`, and `agent-bus-mcp`.
- `agent-bus-core` owns the extracted shared layers for storage adapters,
  channels, settings/models, token helpers, validation, and typed ops.
- `rust-cli` still owns runtime startup and the large transport surfaces in
  `commands.rs`, `http.rs`, and `mcp.rs`.
- The surface crates are wrapper crates over `rust-cli`, so the runtime split
  is still incomplete.
- Build and deploy scripts still treat `rust-cli` as the authoritative crate
  root.
- `thread_id` is implemented as message metadata plus filtering/indexing, but
  there is not yet a first-class thread membership or thread-summary model.
- Task queue payloads are still raw strings rather than validated task cards.
- Pending acknowledgements exist with TTL/stale tracking, but not with
  first-class deadlines/reminders/escalation workflows.

## Current optimization opportunities

1. **Finish the operational crate split** so the CLI/HTTP/MCP surface crates stop depending on `rust-cli` and the scripts stop targeting `rust-cli` directly.
2. **Indexed repo/session reads** so summaries and dedup flows stop over-fetching.
3. **Server-side summarization** for inbox/session rollups instead of replaying raw message history.
4. **Structured task cards** so multi-agent work carries repo/path/priority metadata without parsing free text.
5. **protobuf or A2A adapters** only after repo/session/ownership semantics are stable.

### Guidelines compliance

- M-HOTPATH: Benchmarked before and after; batch function targets the actual hot path
- M-OOBE: Crate builds with `cargo build` alone
- M-STATIC-VERIFICATION: Clippy pedantic + restriction lints enabled
- M-CANONICAL-DOCS: All public Rust functions have doc comments
- M-UNSAFE: No codec-specific unsafe path remains in the removed Python bridge

## Performance/LLM-utilization priorities
1. Keep `compact` and structured events as the default machine path.
2. Add explicit `encoding` choices only where they reduce parsing overhead.
3. Preserve canonical JSON for fallback and manual debugging.
4. Benchmark by measuring p50/p95 watch/read latency and message throughput before changing default encodings or adapters.
