---
name: gemini-agent-hub
description: Use when Gemini CLI is configured with the local Agent Hub MCP server and needs to coordinate large-context analysis, search, or secondary review work with Codex, Claude, or other local agents.
---

# Gemini Agent Hub

Use `agentHub` when Gemini is acting as an analysis or second-opinion agent.

## Startup

1. Call `set_presence` with agent `gemini`.
2. Advertise capabilities such as `analysis`, `search`, `large-context`, or `review`.

## Operating pattern

- Use `list_messages` to find current asks or pending handoffs.
- Use `post_message` with topic `status`, `question`, `review`, or `handoff`.
- Prefer short summaries over long dumps.
- If analysis is blocking another agent, send a direct message to that agent rather than broadcasting.

## Safe usage

- Do not send raw secrets, tokens, or full credential-bearing URLs.
- Prefer `localhost` and explicit file paths.
