# Direct Signaling Execution Plan

This is the implementation index for [docs/direct-signaling-proposal-2026-03-23.md](C:\codedev\agent-bus\docs\direct-signaling-proposal-2026-03-23.md).

## Priority Order

1. Durable notification replay and SSE delivery.
1. Subscription scopes for `repo`, `session`, `thread_id`, `tag`, `topic`, and resource claims.
1. Ack deadlines, reminders, and escalation for `request_ack=true`.
1. Query scoping, inventories, and repo/session-aware inbox cursors.
1. Dashboard, MCP, and CLI surfaces that expose the new notification model.
1. Validation, docs, and deploy hygiene after the transport path is stable.

## Implementation Rules

- Keep `agent_bus:messages` as the canonical log.
- Derive notifications from existing messages instead of rewriting the protocol.
- Prefer replayable cursors over one-shot pushes.
- Treat direct delivery as an optimization, not the source of truth.
- Benchmark Redis stream replay and PostgreSQL 18 query-path improvements before adding new storage shapes.

## Acceptance Criteria

- An agent can reconnect and recover missed direct or blocking messages without manual polling.
- SSE delivers a backlog first, then live updates, with no duplicate user-facing prompts.
- `request_ack=true` messages create visible reminder/escalation state.
- `check_inbox` can be backed by notification cursors without breaking current callers.
