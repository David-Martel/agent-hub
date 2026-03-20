# AGENT PROTOCOL — Analysis Agent (mandatory)

**ID:** {{AGENT_ID}}  **Session:** {{SESSION_ID}}  **Repo:** {{REPO_PATH}}

---

## Bus Communication

Use HTTP (`http://localhost:8400`) — faster than MCP for subprocess calls.

### Post a finding
```bash
curl -s -X POST http://localhost:8400/messages \
  -H "Content-Type: application/json" \
  -d '{
    "sender":"{{AGENT_ID}}",
    "recipient":"{{ORCHESTRATOR}}",
    "topic":"{{REPO}}-findings",
    "body":"FINDING: <one-line description>\nSEVERITY: <CRITICAL|HIGH|MEDIUM|LOW>\nDETAIL: <supporting evidence>",
    "tags":["repo:{{REPO}}","session:{{SESSION_ID}}"],
    "schema":"finding"
  }'
```

### Check inbox (every 3 tool calls)
```bash
curl -s "http://localhost:8400/messages?agent={{AGENT_ID}}&since=5&limit=5"
```

### Claim a file before reading
```bash
curl -s -X POST http://localhost:8400/channels/arbitrate/{{FILE}} \
  -H "Content-Type: application/json" \
  -d '{"agent":"{{AGENT_ID}}","priority_argument":"analysis read"}'
```

### Signal completion
```bash
curl -s -X POST http://localhost:8400/messages \
  -H "Content-Type: application/json" \
  -d '{
    "sender":"{{AGENT_ID}}",
    "recipient":"{{ORCHESTRATOR}}",
    "topic":"{{REPO}}-status",
    "body":"COMPLETE: <N> findings. CRITICAL=<n> HIGH=<n> MEDIUM=<n> LOW=<n>",
    "tags":["repo:{{REPO}}","session:{{SESSION_ID}}"],
    "schema":"status"
  }'
```

---

## Rules

- **Schema is mandatory.** Topic `*-findings` → `"schema":"finding"`. Topic `*-status` → `"schema":"status"`.
- **Batch findings.** Group 3–5 related findings per message; max 2000 chars per body.
- **Always tag** with `repo:{{REPO}}` and `session:{{SESSION_ID}}`.
- **Claim before reading** any file another agent may modify.
- **Send COMPLETE** when done, then poll inbox every 3 calls for follow-up tasks.
- **No panics.** If the bus is unreachable, log locally and continue — do not abort.
