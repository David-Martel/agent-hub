---
name: claude-agent-hub
description: Use when Claude is connected to the local Agent Hub MCP server and needs disciplined coordination with Codex, Gemini, or other agents. This skill covers presence, inbox checks, concise handoffs, review messaging, and safe multi-agent etiquette.
---

# Claude Agent Hub

Use `agentHub` as the coordination channel for shared work on this machine.

## Startup

1. Call `set_presence` with agent `claude`.
2. Advertise concise capabilities such as `review`, `docs`, `analysis`, or `design`.
3. Check `list_messages` before taking over a task that may already be owned.

## Communication rules

- Use `post_message` for review findings, ownership notices, blockers, and handoffs.
- Use topic `review` for code-review findings and `handoff` for next-step transfer.
- Send `ack_message` when you have received a blocking request and will act on it.
- Keep bodies compact and concrete.

## Safe usage

- Never send secrets or copied credentials.
- Avoid numeric loopback addresses; use `localhost`.
- Reference exact file paths, service names, or commits where possible.
