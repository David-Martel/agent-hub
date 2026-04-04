# Phase 3: Crate Split Execution Plan (2026-04-04)

Code-grounded dependency analysis for completing the workspace split.

## Blockers Identified

1. **http.rs -> mcp.rs (MCP-HTTP bridge)**: `http.rs` imports `AgentBusMcpServer`
   for the `/mcp` endpoint. Needs shared tool dispatch in core.

2. **Shared `run()` in lib.rs**: All three entry points funnel through one
   `run()` function (lines 187-684) that parses the full CLI enum. HTTP and MCP
   entries inject default args but still require clap + full Cmd dispatch.

3. **`clap::ValueEnum` in core's `Encoding` enum**: Forces clap as a dependency
   of `agent-bus-core`. Gate behind `#[cfg_attr(feature = "cli", derive(ValueEnum))]`.

4. **`server_mode.rs` is CLI-only**: Only consumed by `commands.rs`. Move to
   `agent-bus-cli`.

5. **lib.rs owns PgWriter lifecycle + startup announce**: Extract shared
   `bootstrap()` into core.

## Migration Sequence

### Step 1: Preparatory refactoring in core (LOW RISK)
- Extract `McpToolDispatch` into core (tool defs + dispatch, no rmcp types)
- Add `bootstrap()` to core (PgWriter init, tracing, startup announce)
- Gate `clap::ValueEnum` behind feature flag on `Encoding`

### Step 2: Split agent-bus-mcp (LOW RISK, do FIRST)
- MCP has narrowest dependency surface
- Only needs core ops + rmcp
- No cross-deps with http.rs or commands.rs

### Step 3: Split agent-bus-http (MEDIUM RISK)
- One cross-dep: MCP-HTTP bridge -> resolved by McpToolDispatch in core
- Largest file (2,774 lines) but mechanically straightforward

### Step 4: Split agent-bus-cli (MEDIUM-HIGH RISK, do LAST)
- Deepest dependency graph (commands.rs touches every op)
- `Serve` command currently starts HTTP/MCP inline
- Key decision: exec binary vs library dep vs remove serve from CLI

### Step 5: Deprecate rust-cli as facade
- Optional "fat binary" or remove entirely

## Key Design Decision: McpToolDispatch

Split `AgentBusMcpServer` into:
- **Core tool dispatch** (`McpToolDispatch`): tool definitions + `call_tool(name, args) -> Result<Value>`. No rmcp dependency.
- **rmcp adapter** (mcp crate): `impl ServerHandler for AgentBusMcpServer` wraps `McpToolDispatch`.
- **HTTP adapter** (http crate): `handle_mcp_http` uses `McpToolDispatch` directly.

## Key Design Decision: CLI Serve Command

Options for `agent-bus serve --transport http`:
- **A (recommended)**: Shell out to `agent-bus-http` binary
- **B**: Depend on http crate as library
- **C**: Remove serve from CLI, use dedicated binaries
