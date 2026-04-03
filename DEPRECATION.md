# Python agent-bus-mcp — Removed

**Status:** Enforced as of 2026-03-22. The repository is now Rust-only.

## Outcome

- The deprecated Python package and pytest suite have been removed from this repo.
- The supported runtime is the Rust `agent-bus` workspace, with the primary runtime surfaces still centered in `rust-cli/` as of 2026-04-03.
- PowerShell wrappers remain supported because they call the Rust binary directly.
- Current architecture checkpoint: [`docs/current-status-2026-04-03.md`](./docs/current-status-2026-04-03.md)

## Migration

| Old (Python) | New (Rust) |
|-------------|------------|
| `agent-bus-mcp health` | `agent-bus health` |
| `agent-bus-mcp send ...` | `agent-bus send ...` |
| `agent-bus-mcp read ...` | `agent-bus read ...` |
| `agent-bus-mcp watch ...` | `agent-bus watch ...` |
| `agent-bus-mcp ack ...` | `agent-bus ack ...` |
| `agent-bus-mcp presence ...` | `agent-bus presence ...` |
| `agent-bus-mcp presence-list ...` | `agent-bus presence-list ...` |
| `agent-bus-mcp serve --transport stdio` | `agent-bus serve --transport stdio` |

All flags remain aligned with the Rust CLI surface.

## Supported Code Paths

- `rust-cli/`: primary runtime implementation, CLI, HTTP server, MCP server, benches, and integration tests
- `crates/agent-bus-core/`: shared storage and orchestration logic extracted from the original monolith
- `crates/agent-bus-cli/`, `crates/agent-bus-http/`, `crates/agent-bus-mcp/`: wrapper crates that currently delegate into `rust-cli/`
- `scripts/`: PowerShell wrappers and deployment helpers
- `docs/`: protocol, assessment, and operational guidance

## Removed Code Paths

- `src/agent_bus_mcp/`
- `tests/test_*.py`
- `pyproject.toml`
- `scripts/build-native-codec.ps1`
- `rust/` PyO3 codec crate

## Follow-up

- Keep removing stale Python references from docs and examples when discovered.
- Do not add new Python runtime code back into this repository.
