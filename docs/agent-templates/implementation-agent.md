# AGENT PROTOCOL — Implementation Agent (mandatory)

**ID:** {{AGENT_ID}}  **Session:** {{SESSION_ID}}  **Repo:** {{REPO_PATH}}

---

## Bus Communication

Use HTTP (`http://localhost:8400`) — faster than MCP for subprocess calls.

### Claim a file before editing (required)
```bash
curl -s -X POST http://localhost:8400/channels/arbitrate/{{FILE}} \
  -H "Content-Type: application/json" \
  -d '{
    "agent":"{{AGENT_ID}}",
    "priority_argument":"implementation write — exclusive edit required"
  }'
```
Wait for `"granted":true` before modifying the file. If `"granted":false`, poll
every 30 seconds until the claim is available or ask the orchestrator to arbitrate.

### Post ownership status after claiming
```bash
curl -s -X POST http://localhost:8400/messages \
  -H "Content-Type: application/json" \
  -d '{
    "sender":"{{AGENT_ID}}",
    "recipient":"all",
    "topic":"{{REPO}}-status",
    "body":"CLAIMED: {{FILE}} — beginning implementation",
    "tags":["repo:{{REPO}}","session:{{SESSION_ID}}"],
    "schema":"status"
  }'
```

### Check inbox (every 3 tool calls)
```bash
curl -s "http://localhost:8400/messages?agent={{AGENT_ID}}&since=5&limit=5"
```

### Signal completion
```bash
curl -s -X POST http://localhost:8400/messages \
  -H "Content-Type: application/json" \
  -d '{
    "sender":"{{AGENT_ID}}",
    "recipient":"{{ORCHESTRATOR}}",
    "topic":"{{REPO}}-status",
    "body":"COMPLETE: implemented <summary>. Files changed: <list>. Tests: <pass|fail>.",
    "tags":["repo:{{REPO}}","session:{{SESSION_ID}}"],
    "schema":"status"
  }'
```

---

## Rules

- **Claim before every edit.** No exceptions — other agents may be reading the same file.
- **Release implicitly.** Claims auto-expire; you do not need to manually release.
- **Schema is mandatory.** All status messages use `"schema":"status"`.
- **Always tag** with `repo:{{REPO}}` and `session:{{SESSION_ID}}`.
- **Check inbox** every 3 tool calls — the orchestrator may redirect or cancel your task.
- **Send COMPLETE** with a concrete summary; include test results if applicable.
- **No panics.** Bus unavailability must not block code edits; log and proceed.
