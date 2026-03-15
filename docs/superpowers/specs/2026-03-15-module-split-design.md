# P3 Module Split — Design Spec

> Split `rust-cli/src/main.rs` (~2700 LOC) into focused modules with zero behavior changes.

## Goal

Break the monolithic `main.rs` into 9 focused modules that each have one clear responsibility,
communicate through well-defined `pub(crate)` interfaces, and can be understood independently.

## Modules

| Module | Responsibility | Source Lines |
|--------|---------------|-------------|
| `settings.rs` | Environment config, `Settings::from_env()`, env helpers | 52-126 |
| `models.rs` | `Message`, `Presence`, `Health`, protocol constants | 127-203 |
| `redis_bus.rs` | Redis connect, stream ops, pub/sub, presence, health | 204-513 (Redis parts) |
| `postgres_store.rs` | PG connect, persist, list, probe, storage cache | 204-513 (PG parts) |
| `output.rs` | `Encoding` enum, output formatters, `minimize_value()` | 924-1028 |
| `validation.rs` | `validate_priority()`, `non_empty()`, `parse_metadata_arg()` | 1266-1297 |
| `commands.rs` | CLI command impls, `SendArgs`/`ReadArgs`/`PresenceArgs` | 1299-1510 |
| `mcp.rs` | `AgentBusMcpServer`, `ServerHandler` impl, tool schemas | 1512-1886 |
| `http.rs` | Axum routes, handlers, request/response types, server bootstrap | 1888-2181 |
| `main.rs` | Crate root, `mod` declarations, startup, `main()`, tracing init | ~200 LOC |

## Dependency Graph

```
main.rs → commands, mcp, http (transport selection)
commands → redis_bus, postgres_store, output, validation, models, settings
mcp → redis_bus, postgres_store, models, settings, validation
http → redis_bus, postgres_store, models, settings, validation
redis_bus → models, settings, postgres_store (for dual-write)
postgres_store → models, settings
output → models
validation → (standalone, no internal deps)
```

## Constraints

- Pure extraction — identical compiled binary behavior
- All types `pub(crate)` unless needed externally
- Existing clippy lints and `#[expect]` annotations preserved
- Tests move with their code into per-module `#[cfg(test)] mod tests`
- `redact_url()` moves to `settings.rs` (it operates on settings data)
- `bus_list_messages()` (the facade that tries PG then falls back to Redis) stays in `redis_bus.rs`
