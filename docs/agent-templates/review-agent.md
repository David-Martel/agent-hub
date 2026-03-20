# AGENT PROTOCOL — Review Agent (mandatory)

**ID:** {{AGENT_ID}}  **Session:** {{SESSION_ID}}  **Repo:** {{REPO_PATH}}

---

## Bus Communication

Use HTTP (`http://localhost:8400`) — faster than MCP for subprocess calls.

### Read sibling findings before starting
```bash
curl -s "http://localhost:8400/messages?agent={{ORCHESTRATOR}}&topic={{REPO}}-findings&since=60&limit=50"
```
Process all findings from the analysis wave before writing review verdicts.

### Check your inbox (every 3 tool calls)
```bash
curl -s "http://localhost:8400/messages?agent={{AGENT_ID}}&since=5&limit=5"
```

### Post a review verdict
```bash
curl -s -X POST http://localhost:8400/messages \
  -H "Content-Type: application/json" \
  -d '{
    "sender":"{{AGENT_ID}}",
    "recipient":"{{ORCHESTRATOR}}",
    "topic":"{{REPO}}-review",
    "body":"VERDICT: <APPROVED|NEEDS_REWORK|ESCALATE>\nSCOPE: <file or component>\nRATIONALE: <one paragraph>",
    "tags":["repo:{{REPO}}","session:{{SESSION_ID}}"],
    "schema":"finding"
  }'
```

### Request re-work from a sibling
```bash
curl -s -X POST http://localhost:8400/messages \
  -H "Content-Type: application/json" \
  -d '{
    "sender":"{{AGENT_ID}}",
    "recipient":"<SIBLING_AGENT_ID>",
    "topic":"{{REPO}}-rework",
    "body":"REWORK: <specific change required>\nFILE: <path>\nSEVERITY: <HIGH|MEDIUM|LOW>",
    "tags":["repo:{{REPO}}","session:{{SESSION_ID}}"],
    "schema":"finding"
  }'
```

### Signal completion
```bash
curl -s -X POST http://localhost:8400/messages \
  -H "Content-Type: application/json" \
  -d '{
    "sender":"{{AGENT_ID}}",
    "recipient":"{{ORCHESTRATOR}}",
    "topic":"{{REPO}}-status",
    "body":"COMPLETE: review done. APPROVED=<n> NEEDS_REWORK=<n> ESCALATED=<n>",
    "tags":["repo:{{REPO}}","session:{{SESSION_ID}}"],
    "schema":"status"
  }'
```

---

## Rules

- **Read all findings first.** Do not begin verdicts until sibling findings are loaded.
- **One verdict per logical unit** (file, module, or feature). Do not combine unrelated verdicts.
- **Schema is mandatory.** Verdicts use `"schema":"finding"`; status uses `"schema":"status"`.
- **Always tag** with `repo:{{REPO}}` and `session:{{SESSION_ID}}`.
- **Escalate blockers.** Use `VERDICT: ESCALATE` for security issues or architectural conflicts.
- **Send COMPLETE** with verdict counts when done, then poll for follow-up tasks.
