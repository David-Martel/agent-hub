# Agent Prompt Templates

Pre-built system prompt fragments for integrating agents with the Agent Bus.
Inject the relevant template into an agent's system prompt at dispatch time,
replacing `{{PLACEHOLDERS}}` with runtime values.

## Templates

| File | Role | Key Responsibilities |
|------|------|----------------------|
| `analysis-agent.md` | Analyzer / auditor | Post findings, claim files before reading, send COMPLETE summary |
| `implementation-agent.md` | Coder / writer | Claim files before editing, post ownership status, send COMPLETE |
| `review-agent.md` | Code/output reviewer | Read sibling findings, post review verdicts, request re-work |
| `orchestrator-agent.md` | Dispatcher / coordinator | Dispatch waves, monitor bus, relay handoffs between agents |

## Placeholder Reference

| Placeholder | Example Value | Description |
|-------------|---------------|-------------|
| `{{AGENT_ID}}` | `analyzer-01` | Unique ID for this agent instance |
| `{{SESSION_ID}}` | `sess-abc123` | Session / run identifier |
| `{{REPO_PATH}}` | `/c/codedev/myrepo` | Absolute path to the target repo |
| `{{REPO}}` | `myrepo` | Short repo name (for topic names and tags) |
| `{{ORCHESTRATOR}}` | `orchestrator` | Agent ID of the coordinating orchestrator |
| `{{FILE}}` | `src/main.rs` | File path when claiming ownership |
| `{{GROUP}}` | `wave-1` | Group name for multi-agent coordination |

## Usage

```bash
# Inject template at dispatch time (bash example)
TEMPLATE=$(cat docs/agent-templates/analysis-agent.md)
TEMPLATE=${TEMPLATE//\{\{AGENT_ID\}\}/analyzer-01}
TEMPLATE=${TEMPLATE//\{\{SESSION_ID\}\}/sess-abc123}
TEMPLATE=${TEMPLATE//\{\{REPO_PATH\}\}/\/c\/codedev\/myrepo}
TEMPLATE=${TEMPLATE//\{\{REPO\}\}/myrepo}
TEMPLATE=${TEMPLATE//\{\{ORCHESTRATOR\}\}/orchestrator}
# Prepend $TEMPLATE to the agent's system prompt
```

## Bus Endpoints (quick reference)

All templates use the HTTP REST API (`http://localhost:8400`) — faster than MCP
for subagent subprocess calls. The MCP transport (`serve --transport stdio`) is
for LLM tool integrations where the model calls tools directly. The HTTP/SSE
runtime is `agent-bus-http.exe`; use it for replay and smoke tests.

| Operation | Endpoint |
|-----------|----------|
| Post message | `POST /messages` |
| Read inbox | `GET /messages?agent=ID&since=N&limit=N` |
| Replay notifications | `GET /notifications/{agent}?history=N&since_id=STREAM_ID` |
| Claim resource | `POST /channels/arbitrate/RESOURCE` |
| Check health | `GET /health` |
| Batch send | `POST /messages/batch` |
| Batch ack | `POST /ack/batch` |
| SSE stream | `GET /events/{agent}?history=N&since_id=STREAM_ID` |

## Operational Notes

- If Redis is not available on a new machine, install the maintained
  [redis-windows](https://github.com/redis-windows/redis-windows) build.
- `GET /notifications/{agent}` is the reconnect path for durable direct-message
  notifications, while `GET /events/{agent}` stays the live SSE stream with
  backlog replay and `Last-Event-ID` support.
- Real multi-agent runs work best when every message carries `repo:<name>` and
  shared planning uses a stable `thread_id`; broad unfiltered reads create
  avoidable inbox noise during multi-repo sessions.
