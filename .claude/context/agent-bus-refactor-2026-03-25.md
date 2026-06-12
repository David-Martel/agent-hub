# Agent Bus Context: Structural Refactor Sprint (2026-03-25)

## State

Branch `refactor/ops-expansion` at `fab96b3`, 14 commits ahead of `main` (3797212).

Phase 1 of the structural refactor is **complete**: the shared ops layer now has 7 modules covering all transport-agnostic orchestration. Phase 2 (transport normalization) is next.

401 unit tests passing, clippy clean, no uncommitted changes.

## Ops Layer (Complete)

```
rust-cli/src/ops/
  mod.rs       -- MessageFilters, scoped_required_tags, re-exports
  message.rs   -- PostMessageRequest, post_message, post_ack, set_presence, list_messages_*
  inbox.rs     -- CheckInboxRequest, check_inbox, CompactContextRequest, compact_context
  channel.rs   -- PostDirectRequest, ReadDirectRequest, PostGroupRequest, escalate, etc.
  claim.rs     -- ClaimResourceRequest, renew, release, resolve, list_claims, parse_lease_mode
  task.rs      -- PushTaskRequest, push_task, pull_task, peek_tasks
  admin.rs     -- ServerMode, ServerControlStatus, current_timestamp
```

All three transport modules (commands.rs, http.rs, mcp.rs) now call ops layer exclusively. Zero direct `channels::` calls remain in transports.

## Commits on Branch

```
fab96b3 docs: add structural refactor and roadmap completion plan
e6c021e refactor(http): import admin types from ops layer
413c92e refactor(commands): use ops layer for task queue commands
a6ee320 refactor(ops): add typed task queue operations layer
8fce882 refactor(mcp): use ops layer for channel and claim tools
f1c66ff refactor(http): use ops layer for channel and claim handlers
9c40a4d refactor(commands): use ops layer for channel and claim commands
000f956 refactor(ops): add typed channel and claim operations layer
e8e0e7d refactor(commands): use ops::inbox for compact_context
bc0ad4c refactor(mcp): use ops::inbox for check_inbox tool
67ff5bc feat(ops): add check_inbox and compact_context inbox operations
e24ffb6 refactor(ops): extract message operations to ops/message
a4fedca refactor(ops): convert ops.rs to directory module
3797212 docs: add design proposals and update project documentation  [main HEAD]
```

## Plan Reference

Full plan: `docs/superpowers/plans/2026-03-25-structural-refactor-and-roadmap.md`

## Next Steps

- **Task 4 (Phase 2)**: Normalize transport boundaries — audit remaining `redis_bus::` calls in transports, split `output.rs` into shared + CLI-only layers, consolidate JSON response shaping
- **Task 9 (Phase 3)**: Workspace split into `agent-bus-core`, `agent-bus-cli`, `agent-bus-http`, `agent-bus-mcp` crates
- **Tasks 5-8**: Feature work (P0-P3 roadmap items) — can proceed in parallel with Task 9 after Task 4

## Decisions

1. **ops/ directory module** over single ops.rs: cleaner separation, one module per capability domain
2. **Thin typed wrappers** over direct refactoring: ops functions delegate to channels.rs/redis_bus.rs rather than moving Redis logic — keeps storage primitives stable during refactor
3. **HealthResult deferred**: `bus_health` already returns `Health` from models.rs; wrapping it adds dead code. Will add when a concrete consumer needs it.
4. **output.rs must split**: http.rs uses TOON formatters — shared formatters go to core crate, CLI-specific formatters stay in CLI crate (Task 4 Step 5)
5. **monitor.rs and mcp_discovery.rs are CLI-only**: go to CLI crate during workspace split, not core
