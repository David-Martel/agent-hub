# Agent Communications

## Purpose

Use Agent Hub as the local coordination layer between Codex, Claude, Gemini, and
other MCP-capable agents on this machine.

Redis handles the live bus path. PostgreSQL keeps durable message and presence
history. The active MCP entrypoint is:

`C:\Users\david\bin\agent-bus.exe serve --transport stdio`

The local HTTP service at `http://localhost:8400` is REST only. It is not a
remote MCP endpoint.

## Stable agent IDs

- `codex`
- `claude`
- `gemini`
- `copilot`
- `euler`
- `pasteur`
- `all`

## Standard workflow

1. Check `bus_health` when the session starts if bus status is uncertain.
2. Call `set_presence` with your agent ID, `online`, and a short capability list.
3. Announce ownership before editing shared infra, prompts, CI, deployment, or service files.
4. Use `post_message` for handoffs, blockers, and ownership changes.
5. Use `list_messages` before making cross-agent assumptions.
6. Use `ack_message` when a blocking handoff has been received and understood.

## Recommended topics

- `status`
- `ownership`
- `handoff`
- `review`
- `bug`
- `deploy`
- `question`
- `ack`

## Message rules

- Keep messages short and concrete.
- Include exact file paths, service names, ports, or commit IDs when relevant.
- Do not send secrets, tokens, passwords, or raw credential strings.
- Do not send large logs; send a short summary plus the path to the log file.
- Prefer `localhost` over numeric loopback addresses in all URLs and examples.

## Ownership examples

- `codex -> all | ownership | taking rust-cli/src/main.rs for postgres storage work`
- `claude -> codex | handoff | review service install script before push`
- `gemini -> all | status | analyzing indexing strategy for agent_bus.messages`

## CLI wrappers

These PowerShell wrappers are the fastest operator path on this machine:

- `scripts/send-agent-bus.ps1`
- `scripts/read-agent-bus.ps1`
- `scripts/watch-agent-bus.ps1`
- `scripts/validate-agent-bus.ps1`
- `scripts/install-agent-hub-service.ps1`

## Suggested capability lists

- Codex: `["mcp","rust","git","service"]`
- Claude: `["mcp","review","docs","design"]`
- Gemini: `["mcp","analysis","search","large-context"]`
- Generic helper agents: `["mcp","coordination"]`
