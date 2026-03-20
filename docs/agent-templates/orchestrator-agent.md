# AGENT PROTOCOL — Orchestrator Agent (mandatory)

**ID:** {{AGENT_ID}}  **Session:** {{SESSION_ID}}  **Repo:** {{REPO_PATH}}

---

## Bus Communication

Use HTTP (`http://localhost:8400`) — faster than MCP for subprocess calls.

### Announce session start
```bash
curl -s -X POST http://localhost:8400/messages \
  -H "Content-Type: application/json" \
  -d '{
    "sender":"{{AGENT_ID}}",
    "recipient":"all",
    "topic":"{{REPO}}-status",
    "body":"SESSION START: {{SESSION_ID}} — dispatching wave 1 for {{REPO}}",
    "tags":["repo:{{REPO}}","session:{{SESSION_ID}}"],
    "schema":"status"
  }'
```

### Dispatch a wave (batch send)
```bash
curl -s -X POST http://localhost:8400/messages/batch \
  -H "Content-Type: application/json" \
  -d '{
    "messages": [
      {
        "sender":"{{AGENT_ID}}","recipient":"analyzer-01",
        "topic":"{{REPO}}-task",
        "body":"TASK: analyze src/ for security issues. Report to {{AGENT_ID}} on {{REPO}}-findings.",
        "tags":["repo:{{REPO}}","session:{{SESSION_ID}}","wave:1"]
      },
      {
        "sender":"{{AGENT_ID}}","recipient":"analyzer-02",
        "topic":"{{REPO}}-task",
        "body":"TASK: analyze tests/ for coverage gaps. Report to {{AGENT_ID}} on {{REPO}}-findings.",
        "tags":["repo:{{REPO}}","session:{{SESSION_ID}}","wave:1"]
      }
    ]
  }'
```

### Monitor bus (poll every 5 tool calls)
```bash
curl -s "http://localhost:8400/messages?agent={{AGENT_ID}}&since=10&limit=20"
```
Count COMPLETE signals. Advance to next wave when all expected agents have reported COMPLETE.

### Relay handoff between waves
```bash
curl -s -X POST http://localhost:8400/messages \
  -H "Content-Type: application/json" \
  -d '{
    "sender":"{{AGENT_ID}}",
    "recipient":"reviewer-01",
    "topic":"{{REPO}}-task",
    "body":"TASK: wave 1 complete. Review findings on topic {{REPO}}-findings from session {{SESSION_ID}}.",
    "tags":["repo:{{REPO}}","session:{{SESSION_ID}}","wave:2"]
  }'
```

### Escalate a conflict
```bash
curl -s -X POST http://localhost:8400/channels/escalate \
  -H "Content-Type: application/json" \
  -d '{
    "agent":"{{AGENT_ID}}",
    "body":"ESCALATION: conflict on resource <FILE> between <AGENT_A> and <AGENT_B>. Arbitration required.",
    "tags":["repo:{{REPO}}","session:{{SESSION_ID}}"]
  }'
```

### Signal session complete
```bash
curl -s -X POST http://localhost:8400/messages \
  -H "Content-Type: application/json" \
  -d '{
    "sender":"{{AGENT_ID}}",
    "recipient":"all",
    "topic":"{{REPO}}-status",
    "body":"SESSION COMPLETE: {{SESSION_ID}}. Waves: <n>. Findings: <n>. Approved: <n>. Rework: <n>.",
    "tags":["repo:{{REPO}}","session:{{SESSION_ID}}"],
    "schema":"status"
  }'
```

---

## Rules

- **Announce before dispatching.** Post SESSION START before spawning any subagent.
- **Wave discipline.** Do not start wave N+1 until all wave N agents post COMPLETE.
- **Monitor continuously.** Poll inbox every 5 tool calls; never go silent for more than 10 calls.
- **Batch dispatches.** Use `/messages/batch` to send all wave tasks in one round-trip.
- **Escalate conflicts immediately.** Do not attempt manual arbitration — use the escalate channel.
- **Always tag** every message with `repo:{{REPO}}` and `session:{{SESSION_ID}}` for traceability.
- **Send SESSION COMPLETE** with aggregate counts as the final bus action.
