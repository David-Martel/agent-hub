# Agent Bus MCP — Interop and Serialization Notes

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

## Rust/PyO3 codec (implemented 2026-03-14)

### Architecture

```
codec.py (abstraction layer — unchanged interface)
  ├── _codec_native.py  (Python fallback — always available)
  └── _codec_native.cp313-win_amd64.pyd  (Rust/PyO3 extension — auto-detected on import)
      └── rust/src/
          ├── lib.rs    (PyO3 module: dumps_compact, dumps_pretty, serialize_stream_payload)
          └── codec.rs  (serde_json serialization + py_to_value conversion)
```

### Functions

| Function | Python fallback | Rust native | Used in |
|----------|----------------|-------------|---------|
| `get_backend_name()` | `"python"` | `"rust-serde"` | `codec_metadata()` |
| `dumps_compact(data)` | `json.dumps(separators)` | `serde_json::to_string` | CLI output, pub/sub events |
| `dumps_pretty(data)` | `json.dumps(indent=2)` | `serde_json::to_string_pretty` | CLI `--encoding json` |
| `serialize_stream_payload(dict)` | per-field Python loop | single Rust call | `post_message()` hot path |

### Build

```powershell
pwsh -NoLogo -NoProfile -File scripts/build-native-codec.ps1 -Release

# Verify backend
.venv/Scripts/python.exe -c "from agent_bus_mcp.codec import codec_metadata; print(codec_metadata())"
# Expected: {'codec': 'pyo3', 'backend': 'rust-serde', 'native': True}
```

### Benchmark results (Python 3.13.7, Rust 1.94, 10K iterations)

| Operation | Python stdlib | Rust/PyO3 | Speedup |
|-----------|--------------|-----------|---------|
| `dumps_compact(message)` | 13.5 us/op | 22.0 us/op | 0.6x (slower — PyO3 crossing overhead) |
| `serialize_stream_payload(message)` | 25.9 us/op | 14.8 us/op | **1.7x** (batched, amortizes crossing) |

**Key insight:** Individual `dumps_compact` calls have too much PyO3 boundary crossing overhead for small payloads. The real win is `serialize_stream_payload`, which batches the entire `post_message()` serialization into a single Rust call, amortizing the Python→Rust crossing cost.

### Future optimization opportunities

1. **Native JSON parsing** for `watch()` inner loop: `parse_compact(json_str) → PyDict`
2. **Schema-validated message construction** in Rust, bypassing Pydantic for high-throughput
3. **Direct Redis wire encoding** from Rust (skip Python dict intermediary entirely)
4. **protobuf wire format** after protocol stabilizes

### Guidelines compliance

- M-HOTPATH: Benchmarked before and after; batch function targets the actual hot path
- M-OOBE: Crate builds with `cargo build` alone
- M-STATIC-VERIFICATION: Clippy pedantic + restriction lints enabled
- M-CANONICAL-DOCS: All public Rust functions have doc comments
- M-UNSAFE: Only PyO3 boundary macros (justified by FFI)

## Performance/LLM-utilization priorities
1. Keep `compact` and structured events as the default machine path.
2. Add explicit `encoding` choices only where they reduce parsing overhead.
3. Preserve canonical JSON for fallback and manual debugging.
4. Benchmark by measuring p50/p95 watch/read latency and message throughput before enabling native codecs by default.
