---
name: generic-agent-hub
description: Use when any MCP-capable coding agent is connected to the local Agent Hub server and needs a concise procedure for presence, ownership, handoffs, acknowledgements, and safe message formatting.
---

# Generic Agent Hub

Use `agentHub` for local multi-agent coordination.

## Standard sequence

1. Call `bus_health`.
2. Call `set_presence`.
3. Call `list_presence` and `list_messages`.
4. Use `post_message` for ownership, handoff, status, and review traffic.
5. Use `ack_message` when a blocking handoff has been accepted.

## Message style

- Short
- Concrete
- Path-aware
- No secrets
- `localhost` only for local URLs
