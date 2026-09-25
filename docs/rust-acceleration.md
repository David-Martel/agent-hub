# Rust Build Acceleration & Profiling Guide

Operator reference for accelerated builds, coverage, profiling, and dependency hygiene
in the `agent-bus` workspace on **Windows (x86_64-pc-windows-msvc)**.

Run all `cargo` commands from the **workspace root** (where `.cargo/config.toml` lives).
The sccache wrapper and lld-link linker activate automatically.

---

## Quick Reference

| Alias | Command | Needs install? |
|---|---|---|
| `cargo ab-build` | Build all three binaries | No |
| `cargo ab-fast` | Fast-release build (thin LTO off, incremental) | No |
| `cargo ab-test` | Workspace unit tests | No |
| `cargo ab-itest` | `#[ignore]`d backend tests; needs DISPOSABLE backends via `AGENT_BUS_TEST_*` (live ports refused) | No |
| `cargo ab-clippy` | Clippy pedantic + restriction lints | No |
| `cargo ab-nextest` | nextest test runner | `cargo-nextest` |
| `cargo ab-cov` | LLVM coverage report → `lcov.info` | `cargo-llvm-cov` |
| `cargo ab-bench` | Criterion benchmarks | No |
| `cargo ab-audit` | Security vulnerability audit | `cargo-audit` |
| `cargo ab-machete` | Find unused dependencies | `cargo-machete` |
| `cargo ab-flame -- <args>` | Flamegraph CPU profile | `flamegraph` |

Install all missing tools at once:

```powershell
pwsh -NoLogo -NoProfile -File scripts/install-rust-tooling.ps1
```

or on Linux/WSL:

```bash
bash scripts/install-rust-tooling.sh
```

---

## Tooling Details

### sccache — Compiler Cache (ALREADY CONFIGURED)

**What it does**: Caches compiled Rust artifacts across clean builds and CI runs.
First build is normal speed; subsequent builds for unchanged code skip recompilation.
Cache lives at `T:\RustCache` (configured globally for this machine).

**Status**: Already wired via `.cargo/config.toml` → `scripts/rustc-wrapper.cmd`.
No action needed.

**Bypass** (for debugging compiler output):
```powershell
$env:RUSTC_WRAPPER = ""; cargo build
```

**Cache stats**:
```powershell
sccache --show-stats
sccache --zero-stats   # reset counters
```

---

### lld-link — Fast Linker (ALREADY CONFIGURED)

**What it does**: Microsoft's `lld-link` (LLVM linker) links 2–5× faster than
the MSVC `link.exe` default. Particularly noticeable on incremental rebuilds
where only the final binary link step runs.

**Status**: Already configured for `x86_64-pc-windows-msvc` in `.cargo/config.toml`.
No action needed.

**Install** (if not present — ships with LLVM, or via `winget`):
```powershell
winget install LLVM.LLVM
```

---

### cargo-nextest — Faster Test Runner

**What it does**: Runs each test in its own process, enables parallel execution with
better isolation, produces structured JSON output, and gives much cleaner pass/fail
reporting. Typically 20–60% faster than `cargo test` on multi-crate workspaces.

**Status**: Alias `ab-nextest` is defined. Install required.

**Install**:
```powershell
cargo install cargo-nextest --locked
```

**Usage**:
```powershell
cargo ab-nextest                          # all workspace tests
cargo nextest run -p agent-bus            # single crate
cargo nextest run --test http_integration_test --run-ignored ignored-only -j 1   # 48 #[ignore]d backend tests; without the flag only the 4 non-ignored ones run (measured 2026-09-25, nextest 0.9.140)
```

**nextest config** (create `.config/nextest.toml` to customize):
```toml
[profile.default]
test-threads = "num-cpus"
retries = 0
```

---

### cargo-llvm-cov — Code Coverage

**What it does**: Instruments binaries with LLVM's source-based coverage and produces
LCOV, HTML, or JSON reports. Much more accurate than `grcov`/`tarpaulin`.

**Install**:
```powershell
cargo install cargo-llvm-cov --locked
rustup component add llvm-tools-preview   # required component
```

**Usage**:
```powershell
# Generate lcov.info (used by CI / coverage badges)
cargo ab-cov

# Interactive HTML report (opens browser)
cargo llvm-cov --workspace --open

# Per-crate summary to stdout
cargo llvm-cov --workspace --summary-only

# Fail if coverage drops below threshold
cargo llvm-cov --workspace --fail-under-lines 80
```

**CI integration**: The `ab-cov` alias writes `lcov.info` to the workspace root,
compatible with Codecov/Coveralls upload steps.

---

### flamegraph / cargo-flamegraph — CPU Profiling

**What it does**: Samples the call stack at high frequency while the binary runs,
then renders an SVG flamegraph you can open in a browser. Best for identifying
CPU hotpaths (serialization, encoding, Redis command batching).

**Install**:
```powershell
cargo install flamegraph --locked
```

On Windows, flamegraph uses `dtrace` (available in recent Windows 11 builds, or
via the Windows Performance Toolkit). If `dtrace` is unavailable, use `samply`
instead (see below).

**Usage** (with the `[profile.profiling]` profile — keeps debug symbols):
```powershell
# Profile the agent-bus binary with a workload
cargo ab-flame -- send --from agent-a --to agent-b --body "hello" --count 1000
# → produces flamegraph.svg in current directory

# Inspect with any browser
start flamegraph.svg
```

**Profile details** (`[profile.profiling]` in `Cargo.toml`):
- Inherits `release` (full optimizations — realistic hotpaths)
- `debug = true` — full DWARF symbols for frame resolution
- `strip = false` — symbols retained in binary

---

### samply — Windows-Native Profiler (Recommended over flamegraph on Windows)

**What it does**: Uses Windows ETW (Event Tracing for Windows) to collect CPU samples,
then opens the Firefox Profiler UI in your browser for interactive exploration.
Works without `dtrace`. Best choice for async Tokio profiling on Windows.

**Install**:
```powershell
cargo install samply --locked
```

**Usage**:
```powershell
# Build with profiling symbols first
cargo build --profile profiling -p agent-bus

# Run under samply (opens Firefox Profiler automatically)
samply record .\target\profiling\agent-bus.exe send --from a --to b --body test
```

**Tokio async tracing**: Combine with `tokio-console` for async task visibility
(see below).

---

### tokio-console — Async Task Debugger

**What it does**: A terminal dashboard that connects to a running Tokio runtime
and shows live task state, wakeups, and poll counts. Essential for diagnosing
async stalls, task starvation, and unexpected blocking.

**Install**:
```powershell
cargo install tokio-console --locked
```

**Enable in the binary** (requires adding `console-subscriber` to `agent-bus-cli`):
```toml
# In crates/agent-bus-cli/Cargo.toml [dev-dependencies] or optional feature:
console-subscriber = "0.4"
```

```rust
// In main.rs, replace tracing_subscriber init:
console_subscriber::init();
```

**Usage**:
```powershell
# Terminal 1: run binary with console enabled
$env:TOKIO_CONSOLE_BIND = "127.0.0.1:6669"
cargo run -p agent-bus -- serve --transport http

# Terminal 2: connect console
tokio-console
```

Note: `console-subscriber` should be guarded by a feature flag (e.g., `tokio-debug`)
and not shipped in release builds.

---

### cargo-machete — Unused Dependency Finder

**What it does**: Statically scans `Cargo.toml` files and identifies dependencies
that are listed but never referenced in source code. Fast (no compilation needed).

**Install**:
```powershell
cargo install cargo-machete --locked
```

**Usage**:
```powershell
cargo ab-machete           # alias: scans workspace
cargo machete --fix        # auto-remove unused deps (review diff before commit)
```

---

### cargo-udeps — Unused Dependency Finder (nightly, more thorough)

**What it does**: Like `cargo-machete` but uses the compiler's internal tracking,
so it catches more cases (transitive, feature-gated). Requires nightly Rust.

**Install**:
```powershell
rustup toolchain install nightly
cargo +nightly install cargo-udeps --locked
```

**Usage**:
```powershell
cargo +nightly udeps --workspace
cargo +nightly udeps --workspace --all-targets
```

Run `cargo-machete` first (fast, no toolchain switch); use `cargo-udeps` for
a thorough check before a release.

---

### cargo-audit — Security Vulnerability Audit (ALREADY IN CI)

**What it does**: Checks all dependencies against the RustSec Advisory Database
for known CVEs and unsound crates.

**Status**: Already runs in CI (pre-push hook via Lefthook). The `ab-audit` alias
provides local on-demand runs.

**Install** (if not already present):
```powershell
cargo install cargo-audit --locked
```

**Usage**:
```powershell
cargo ab-audit             # audit workspace dependencies
cargo audit fix            # auto-update vulnerable deps (review carefully)
cargo audit --deny warnings  # strict mode (fails on any advisory)
```

---

### Cranelift Codegen Backend — Faster Debug Builds (Experimental)

**What it does**: Replaces LLVM with Cranelift for the `dev` profile, producing
faster compilation at the cost of slightly worse runtime performance in debug
builds. Zero effect on `release`/`fast-release`/`profiling` profiles (those still
use LLVM). Particularly helpful for tight edit-compile cycles.

**Status**: Experimental on Windows — support improved in Rust 1.82+. Worth trying
if debug build times are a bottleneck.

**Install**:
```powershell
rustup component add rustc-codegen-cranelift-preview --toolchain stable
```

**Enable in `.cargo/config.toml`** (add under `[unstable]` — requires nightly or
`-Z` flag, so evaluate carefully):
```toml
[unstable]
codegen-backend = true

[profile.dev]
codegen-backend = "cranelift"
```

**Current status**: The workspace already has an optimized `[profile.dev]` with
`codegen-units = 256` and `incremental = true`, which recovers most of the same
latency gain. Try Cranelift only if incremental builds still feel slow.

---

## Build Profiles Summary

| Profile | Opt | LTO | Debug | Strip | Use case |
|---|---|---|---|---|---|
| `dev` | 0 | off | 1 | no | Daily development, tests |
| `test` | 0 | off | 1 | no | `cargo test` |
| `release` | 3 | thin | no | yes | Production deploy |
| `fast-release` | 3 | off | 1 | debuginfo | Local perf builds, CI fast path |
| `profiling` | 3 | thin | full | no | flamegraph, samply, perf |

---

## Recommended Workflow

### Daily development
```powershell
cargo ab-fast          # fast incremental build
cargo ab-test          # unit tests (no services needed)
cargo ab-clippy        # lint check
```

### Before a PR
```powershell
cargo ab-nextest       # faster full test run
cargo ab-audit         # security check
cargo ab-machete       # unused dep check
cargo fmt --all --check
```

### Investigating a performance regression
```powershell
# 1. Build with profiling symbols
cargo build --profile profiling -p agent-bus

# 2. Profile with samply (Windows ETW, no dtrace needed)
samply record .\target\profiling\agent-bus.exe <workload args>

# 3. Or generate a flamegraph (if dtrace available)
cargo ab-flame -- <workload args>
```

### Coverage report
```powershell
cargo llvm-cov --workspace --open   # HTML in browser
cargo ab-cov                         # lcov.info for CI upload
```
