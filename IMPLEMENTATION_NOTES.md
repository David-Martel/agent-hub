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

## Current optimization opportunities

1. **Indexed repo/session reads** so summaries and dedup flows stop over-fetching.
2. **Server-side summarization** for inbox/session rollups instead of replaying raw message history.
3. **Structured task cards** so multi-agent work carries repo/path/priority metadata without parsing free text.
4. **protobuf or A2A adapters** only after repo/session/ownership semantics are stable.

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
