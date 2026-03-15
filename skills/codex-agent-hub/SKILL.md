---
name: codex-agent-hub
description: Use when Codex is working in a repository that has the local Agent Hub MCP server configured. This skill covers how Codex should use the agentHub MCP tools for presence, ownership, handoffs, acknowledgements, and inbox checks while collaborating with Claude, Gemini, or other local agents.
---

# Codex Agent Hub

Use the `agentHub` MCP server before making assumptions about parallel work.

## Startup

1. Call `bus_health` if server state is unclear.
2. Call `set_presence` with agent `codex`.
3. Use capabilities such as `mcp`, `rust`, `git`, `service`, or `review`.

## Coordination rules

- Announce ownership before editing shared infra, service, prompt, CI, or config files.
- Use `post_message` with topic `ownership`, `handoff`, `status`, `review`, or `bug`.
- Use `list_messages` before picking up a shared task or assuming another agent is idle.
- Use `ack_message` for blocking handoffs.
- Keep messages short and include exact file paths or service names.

## Safe usage

- Do not send secrets or credentials.
- Do not paste large logs into the bus; send a short summary and a log path.
- Prefer `localhost` in any URL or service reference.
