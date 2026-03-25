# Agent Bus Structural Refactor & Roadmap Completion Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete the structural refactor (expand shared ops, normalize transports, split into workspace crates) and implement remaining roadmap items from TODO.md (P0-P6).

**Architecture:** Extract transport-agnostic orchestration from commands.rs (1659 LOC), http.rs (2849 LOC), and mcp.rs (1474 LOC) into typed ops modules. Then split the monolithic `rust-cli` package into a Cargo workspace with `agent-bus-core`, `agent-bus-cli`, `agent-bus-http`, and `agent-bus-mcp` crates. Concurrently fill remaining feature gaps (subscriptions, ack deadlines, task cards, scoped cursors, token summarization, parity tests).

**Tech Stack:** Rust 2024, Tokio, Axum, Redis Streams, PostgreSQL, rmcp, Clap, serde, mimalloc

**Codebase baseline:** ~20,400 LOC across 20 modules, 3 binary targets, 17 MCP tools, 35 CLI subcommands, 26+ HTTP routes.

---

## Scope Overview

This plan covers work from both `TODO.md` (P0-P6 roadmap) and `agents.TODO.md` (structural refactor phases 1-6). The two are deeply intertwined: expanding the ops layer (structural) is a prerequisite for cleanly implementing subscription records (P0), scoped cursors (P1), and task cards (P2).

### Work Streams

| Stream | Source | Phases |
|--------|--------|--------|
| A. Shared ops expansion | agents.TODO Phase 1 | Tasks 1-3 |
| B. Transport normalization | agents.TODO Phase 2 | Task 4 |
| C. Feature: Direct signaling | TODO.md P0 | Task 5 |
| D. Feature: Query model | TODO.md P1 | Task 6 |
| E. Feature: Orchestration | TODO.md P2 | Task 7 |
| F. Feature: Token efficiency | TODO.md P3 | Task 8 |
| G. Workspace split | agents.TODO Phase 3-4 | Task 9 |
| H. Validation matrix | agents.TODO Phase 5, TODO.md P4 | Task 10 |
| I. Docs & packaging | agents.TODO Phase 6, TODO.md P5-P6 | Task 11 |

### Dependency Graph

```
Task 1 (ops/inbox+message) ─┐
Task 2 (ops/channel+claim) ─┼─> Task 4 (normalize) ─> Task 9 (workspace split)
Task 3 (ops/task+admin) ────┘        │                        │
                                     v                        v
Task 5 (P0 signaling) ──────> Task 6 (P1 query) ──> Task 10 (validation)
Task 7 (P2 orchestration) ───────────────────────────────────┘
Task 8 (P3 tokens) ──────────────────────────────────────────┘
                                                              v
                                                     Task 11 (docs)
```

### File Structure (target state)

```
rust-cli/src/
  ops/
    mod.rs          -- re-exports, MessageFilters, scoped_required_tags
    message.rs      -- PostMessageRequest, post_message, list_messages_*
    inbox.rs        -- CheckInboxRequest, compact_context, notification cursor
    channel.rs      -- PostDirectRequest, PostGroupRequest, escalation, channel ops
    claim.rs        -- ClaimRequest, renew/release/resolve, list_claims
    task.rs         -- PushTaskRequest, pull/peek, validated task cards
    admin.rs        -- ServerControlStatus, pause/resume/drain, health
  ops.rs            -- DELETED (replaced by ops/ directory module)
  lib.rs            -- runtime init (PG_WRITER, Tokio, startup announce)
  output.rs         -- shared TOON/compact/minimal formatters (used by CLI + HTTP)
  commands.rs       -- slimmed: arg parse + ops call + CLI output format
  http.rs           -- slimmed: DTO extract + ops call + response convert
  mcp.rs            -- slimmed: schema wire + ops call + MCP result convert
  channels.rs       -- low-level Redis primitives only (no orchestration)
  redis_bus.rs      -- low-level Redis primitives only
  postgres_store.rs -- low-level PG primitives only
  monitor.rs        -- CLI-only: terminal dashboard (stays in CLI crate)
  mcp_discovery.rs  -- CLI-only: Claude config discovery (stays in CLI crate)
  server_mode.rs    -- CLI-only: HTTP client for AGENT_BUS_SERVER_URL mode
  ... (other modules unchanged)
```

---

## Task 1: Expand ops — inbox and message completion (Phase 1A)

**Files:**
- Create: `rust-cli/src/ops/mod.rs`
- Create: `rust-cli/src/ops/message.rs`
- Create: `rust-cli/src/ops/inbox.rs`
- Delete: `rust-cli/src/ops.rs` (content moves into ops/ submodules)
- Modify: `rust-cli/src/commands.rs` (replace direct redis calls with ops)
- Modify: `rust-cli/src/http.rs` (replace direct redis calls with ops)
- Modify: `rust-cli/src/mcp.rs` (move cursor logic to ops/inbox)
- Modify: `rust-cli/src/lib.rs` (update `mod ops` declaration)
- Test: `rust-cli/tests/ops_message_test.rs`
- Test: `rust-cli/tests/ops_inbox_test.rs`

**Context:** Currently `ops.rs` (246 LOC) owns send/ack/presence/read helpers. The inbox cursor logic lives inside `mcp.rs` call_tool handler. The `commands.rs` `cmd_compact_context` function duplicates filtering/compaction logic that HTTP also needs.

### Steps

- [ ] **Step 1: Create ops/ directory module from existing ops.rs**

  Move `rust-cli/src/ops.rs` content into `rust-cli/src/ops/mod.rs`. Add `pub mod message;` and `pub mod inbox;` declarations. Create empty `message.rs` and `inbox.rs` files. Update `lib.rs` — the `mod ops;` declaration works unchanged since Rust resolves `ops/mod.rs` the same way.

  Run: `cd rust-cli && cargo ab-clippy`
  Expected: PASS (no behavior change)

- [ ] **Step 2: Commit scaffold**

  ```bash
  git add rust-cli/src/ops/ rust-cli/src/lib.rs
  git rm rust-cli/src/ops.rs
  git commit -S -m "refactor(ops): convert ops.rs to directory module"
  ```

- [ ] **Step 3: Move message types and functions to ops/message.rs**

  Move `PostMessageRequest`, `AckMessageRequest`, `AckMessageResult`, `PresenceRequest`, `ReadMessagesRequest`, `knock_metadata`, `post_message`, `post_ack`, `set_presence`, `list_messages_history`, `list_messages_live` from `ops/mod.rs` to `ops/message.rs`. Re-export from `mod.rs`.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 4: Commit message extraction**

  ```bash
  git commit -S -am "refactor(ops): extract message operations to ops/message"
  ```

- [ ] **Step 5: Add CheckInboxRequest and notification cursor ops**

  In `ops/inbox.rs`, create:
  - `CheckInboxRequest { agent, cursor_key, limit }`
  - `CheckInboxResult { messages, new_cursor }`
  - `fn check_inbox(conn, settings, request) -> Result<CheckInboxResult>`
  - `CompactContextRequest { agent, filters, since_minutes, max_tokens }`
  - `fn compact_context(settings, request) -> Result<CompactContextResult>`

  Extract the notification-cursor read/update logic currently in `mcp.rs` `call_tool` (the `"check_inbox"` match arm, ~60 lines) into `ops::inbox::check_inbox`.

  Run: `cd rust-cli && cargo ab-clippy && cargo ab-test`
  Expected: PASS

- [ ] **Step 6: Write inbox ops unit test**

  In `rust-cli/tests/ops_inbox_test.rs`: test that `check_inbox` returns messages, advances cursor. Test that `compact_context` respects `max_tokens` limit. These require Redis, so mark `#[ignore]` if run without services.

  Run: `cd rust-cli && cargo test ops_inbox -- --ignored` (with services running)
  Expected: PASS

- [ ] **Step 7: Update mcp.rs to call ops::inbox::check_inbox**

  Replace the inline cursor logic in `mcp.rs` `"check_inbox"` handler with a call to `ops::inbox::check_inbox`. The MCP handler should only parse tool arguments, call the op, and format the result.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 8: Update commands.rs cmd_compact_context to use ops::inbox**

  Replace the inline filtering/compaction in `cmd_compact_context` with `ops::inbox::compact_context`. The command function should only parse args, call the op, and format output.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 9: Commit inbox ops**

  ```bash
  git commit -S -am "feat(ops): extract inbox and compact-context operations"
  ```

---

## Task 2: Expand ops — channels and claims (Phase 1B)

**Files:**
- Create: `rust-cli/src/ops/channel.rs`
- Create: `rust-cli/src/ops/claim.rs`
- Modify: `rust-cli/src/commands.rs` (slim channel/claim commands)
- Modify: `rust-cli/src/http.rs` (slim channel/claim handlers)
- Modify: `rust-cli/src/mcp.rs` (slim channel/claim tool dispatch)
- Modify: `rust-cli/src/channels.rs` (keep only low-level Redis primitives)
- Test: `rust-cli/tests/ops_channel_test.rs`
- Test: `rust-cli/tests/ops_claim_test.rs`

**Context:** `channels.rs` (2577 LOC) contains both low-level Redis stream/hash operations AND orchestration logic (claim_resource_with_options, resolve_claim, post_escalation, arbitration_claim). The three transports each call these directly. The ops layer should own orchestration; channels.rs keeps only storage primitives.

### Steps

- [ ] **Step 1: Create typed channel request/response structs in ops/channel.rs**

  ```rust
  pub(crate) struct PostDirectRequest<'a> {
      pub(crate) from_agent: &'a str,
      pub(crate) to_agent: &'a str,
      pub(crate) topic: &'a str,
      pub(crate) body: &'a str,
      pub(crate) thread_id: Option<&'a str>,
      pub(crate) tags: &'a [String],
  }
  // Similarly: PostGroupRequest, ReadDirectRequest, ReadGroupRequest, EscalateRequest
  ```

  Implement `post_direct`, `read_direct`, `post_group`, `read_group`, `escalate` by delegating to `channels::post_direct`, `channels::read_direct`, etc.

  Run: `cd rust-cli && cargo ab-clippy`
  Expected: PASS

- [ ] **Step 2: Create typed claim request/response structs in ops/claim.rs**

  ```rust
  pub(crate) struct ClaimResourceRequest<'a> {
      pub(crate) resource: &'a str,
      pub(crate) agent: &'a str,
      pub(crate) reason: &'a str,
      pub(crate) options: ClaimOptions,
  }
  // Similarly: RenewClaimRequest, ReleaseClaimRequest, ResolveClaimRequest, ListClaimsRequest
  ```

  Implement by delegating to `channels::claim_resource_with_options`, `channels::renew_claim`, etc.

  Run: `cd rust-cli && cargo ab-clippy`
  Expected: PASS

- [ ] **Step 3: Migrate commands.rs channel/claim functions to use ops**

  Replace `cmd_post_direct`, `cmd_read_direct`, `cmd_post_group`, `cmd_read_group`, `cmd_claim`, `cmd_renew_claim`, `cmd_release_claim`, `cmd_claims`, `cmd_resolve` to call ops layer instead of channels.rs directly.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 4: Migrate http.rs channel/claim handlers to use ops**

  Replace inline channel/claim orchestration in HTTP handlers with ops calls. HTTP handlers should: extract DTO, build ops request, call op, convert result to HTTP response.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 5: Migrate mcp.rs channel/claim tools to use ops**

  Replace inline channel/claim logic in MCP call_tool with ops calls.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 6: Write channel/claim ops tests**

  Unit tests for `ops::channel::post_direct`, `ops::claim::claim_resource`, etc. Require Redis — mark `#[ignore]`.

  Run: `cd rust-cli && cargo test ops_channel ops_claim -- --ignored`
  Expected: PASS

- [ ] **Step 7: Commit**

  ```bash
  git commit -S -am "refactor(ops): extract channel and claim operations"
  ```

---

## Task 3: Expand ops — tasks and admin control (Phase 1C)

**Files:**
- Create: `rust-cli/src/ops/task.rs`
- Create: `rust-cli/src/ops/admin.rs`
- Modify: `rust-cli/src/commands.rs` (slim task/admin commands)
- Modify: `rust-cli/src/http.rs` (move ServerControlStatus to ops/admin)
- Modify: `rust-cli/src/mcp.rs` (if task MCP tools exist)

**Context:** Task queue ops (`push_task`, `pull_task`, `peek_tasks`) are currently inline in `commands.rs` calling `redis_bus` directly. HTTP server control status (`ServerControlStatus`, `ServerMode`, pause/resume/drain) is defined in `http.rs` but is transport-agnostic logic.

### Steps

- [ ] **Step 1: Create ops/task.rs with typed task operations**

  ```rust
  pub(crate) struct PushTaskRequest<'a> {
      pub(crate) agent: &'a str,
      pub(crate) task: &'a str,
  }
  pub(crate) fn push_task(conn, settings, request) -> Result<()>
  pub(crate) fn pull_task(conn, settings, agent) -> Result<Option<String>>
  pub(crate) fn peek_tasks(conn, settings, agent, limit) -> Result<Vec<String>>
  ```

  Run: `cd rust-cli && cargo ab-clippy`
  Expected: PASS

- [ ] **Step 2: Create ops/admin.rs with service control types**

  Move `ServerMode`, `ServerControlStatus` from `http.rs` to `ops/admin.rs`. Add `HealthResult` struct wrapping the health check output. HTTP keeps a thin type alias.

  Run: `cd rust-cli && cargo ab-clippy`
  Expected: PASS

- [ ] **Step 3: Migrate commands.rs and http.rs to use ops/task and ops/admin**

  Replace `cmd_push_task`, `cmd_pull_task`, `cmd_peek_tasks` inline Redis calls with ops calls. Update `http.rs` to import `ServerControlStatus` from ops.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 4: Commit**

  ```bash
  git commit -S -am "refactor(ops): extract task queue and admin control operations"
  ```

---

## Task 4: Normalize transport boundaries (Phase 2)

**Files:**
- Modify: `rust-cli/src/commands.rs`
- Modify: `rust-cli/src/http.rs`
- Modify: `rust-cli/src/mcp.rs`
- Modify: `rust-cli/src/output.rs`

**Context:** After Tasks 1-3, ops modules own orchestration. This task ensures all three transports follow the same pattern: parse input → build typed request → call op → format output. Remove remaining parallel logic.

### Steps

- [ ] **Step 1: Audit remaining direct redis_bus/channels calls in commands.rs**

  Search for `redis_bus::` and `channels::` calls in commands.rs that bypass ops. Move each to the appropriate ops module.

  Run: `cd rust-cli && cargo ab-clippy`
  Expected: PASS

- [ ] **Step 2: Audit remaining direct redis_bus/channels calls in http.rs**

  Same sweep for http.rs. HTTP handlers should only call ops functions and `bus_health` (which is already a read-only query).

  Run: `cd rust-cli && cargo ab-clippy`
  Expected: PASS

- [ ] **Step 3: Audit remaining direct redis_bus/channels calls in mcp.rs**

  Same sweep for mcp.rs.

  Run: `cd rust-cli && cargo ab-clippy`
  Expected: PASS

- [ ] **Step 4: Consolidate JSON response shaping**

  Where HTTP and MCP produce the same semantic result but shape JSON differently without reason, consolidate into a shared result type in ops. Keep transport-specific formatting (e.g., MCP `CallToolResult`, HTTP status codes) in the transport modules.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 5: Split output.rs into shared and CLI-specific layers**

  `http.rs` imports `format_health_toon`, `format_message_toon`, `format_presence_toon` from `output.rs` — these are NOT CLI-only. Split `output.rs` into:
  - `output_core.rs` (or keep in `output.rs`): TOON/compact/minimal formatters shared by CLI and HTTP — stays in core.
  - CLI-specific table/human formatters: only called from `commands.rs`.

  This split is required before the workspace crate split (Task 9) since `agent-bus-http` needs TOON formatters from core.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 6: Commit**

  ```bash
  git commit -S -am "refactor: normalize transport boundaries around shared ops layer"
  ```

---

## Task 5: P0 — Direct signaling completion

**Files:**
- Modify: `rust-cli/src/redis_bus.rs`
- Modify: `rust-cli/src/ops/inbox.rs`
- Modify: `rust-cli/src/ops/message.rs`
- Modify: `rust-cli/src/models.rs`
- Modify: `rust-cli/src/cli.rs`
- Modify: `rust-cli/src/commands.rs`
- Modify: `rust-cli/src/http.rs`
- Modify: `rust-cli/src/mcp.rs`
- Test: `rust-cli/tests/subscription_test.rs`

**Remaining P0 items:**
1. Explicit subscription records (recipient, repo, session, thread, tag, topic, priority, resource scopes)
2. Promote `thread_id` to joinable conversation scope with membership
3. Ack deadlines, reminder delivery, and escalation for `request_ack=true`

### Steps

- [ ] **Step 1: Design subscription model in models.rs**

  Add `Subscription` struct: `{ id, agent, scope_type (recipient|repo|session|thread|tag|topic|priority|resource), scope_value, created_at }`. Store in Redis hash `bus:subscriptions:{agent}`.

  Run: `cd rust-cli && cargo ab-clippy`
  Expected: PASS

- [ ] **Step 2: Implement subscription CRUD in redis_bus.rs**

  `subscribe(conn, agent, scope) -> Result<Subscription>`
  `unsubscribe(conn, agent, subscription_id) -> Result<()>`
  `list_subscriptions(conn, agent) -> Result<Vec<Subscription>>`
  `match_subscriptions(conn, message) -> Result<Vec<String>>` (returns matching agent IDs)

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 3: Add subscription-aware notification dispatch**

  When posting a message, call `match_subscriptions` to find agents with matching scopes. Deliver to their notification streams in addition to the broadcast stream. This replaces the current recipient-only notification path.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 4: Add thread membership**

  In `redis_bus.rs`: `join_thread(conn, agent, thread_id)`, `leave_thread(conn, agent, thread_id)`, `list_thread_members(conn, thread_id)`. Store in Redis set `bus:thread:{thread_id}:members`. Messages with `thread_id` auto-notify all members.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 5: Add ack deadline tracking**

  When `request_ack=true`, store deadline in Redis sorted set `bus:ack_deadlines` with score = deadline timestamp. Add `check_ack_deadlines` function that returns overdue message IDs.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 6: Add ack reminder/escalation background task**

  In the HTTP server startup, spawn a Tokio task that polls `check_ack_deadlines` every 30s. For overdue acks: post a reminder to the original recipient. After 2 reminders: post an escalation to the orchestrator channel.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 7: Add CLI/HTTP/MCP surface for subscriptions**

  CLI: `subscribe`, `unsubscribe`, `list-subscriptions` subcommands.
  HTTP: `POST /subscriptions`, `DELETE /subscriptions/:id`, `GET /subscriptions/:agent`.
  MCP: `subscribe`, `unsubscribe` tools.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 8: Write integration tests**

  Test subscription matching, thread membership notification, ack deadline escalation.

  Run: `cd rust-cli && cargo test subscription -- --ignored`
  Expected: PASS

- [ ] **Step 9: Commit**

  ```bash
  git commit -S -am "feat(signaling): add subscriptions, thread membership, and ack deadlines"
  ```

---

## Task 6: P1 — Query model and cross-repo awareness

**Files:**
- Modify: `rust-cli/src/postgres_store.rs`
- Modify: `rust-cli/src/ops/inbox.rs`
- Modify: `rust-cli/src/ops/message.rs`
- Modify: `rust-cli/src/redis_bus.rs`
- Modify: `rust-cli/src/cli.rs`
- Modify: `rust-cli/src/commands.rs`
- Modify: `rust-cli/src/http.rs`
- Test: `rust-cli/tests/query_model_test.rs`

**Remaining P1 items:**
1. Route tagged queries through PostgreSQL indexes (stop over-fetching)
2. Repo/session-scoped inbox cursors
3. Repo/session inventory commands
4. Thread summaries/compaction

### Steps

- [ ] **Step 1: Route tagged queries through PG when available**

  In `ops/message.rs`, `list_messages_history` already tries PG. Add a `pg_query_by_tags` path in `postgres_store.rs` that uses the GIN index on tags for `session-summary`, `dedup`, and `compact-context` instead of fetching all messages then filtering client-side.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 2: Add scoped inbox cursors**

  Replace the single global cursor per agent (`bus:notify:cursor:{agent}`) with scoped cursors: `bus:notify:cursor:{agent}:repo:{repo}`, `bus:notify:cursor:{agent}:session:{session}`. Update `ops/inbox.rs` to accept an optional scope.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 3: Add inventory commands**

  CLI: `repos` (list active repos), `sessions` (list active sessions), `agents-by-repo {repo}`, `claims-by-repo {repo}`.
  HTTP: `GET /repos`, `GET /sessions`, `GET /repos/:name/agents`, `GET /repos/:name/claims`.
  Implementation: scan PG tags or Redis keys for `repo:*` / `session:*` patterns.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 4: Add thread summary/compaction**

  `compact-thread --thread-id X` reads all messages in the thread, produces a token-efficient summary. Add `ops/inbox::compact_thread`. Expose via CLI, HTTP (`GET /threads/:id/summary`), and MCP.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 5: Write tests and commit**

  ```bash
  git commit -S -am "feat(query): add PG-routed tag queries, scoped cursors, inventory, and thread summaries"
  ```

---

## Task 7: P2 — Cross-agent orchestration

**Files:**
- Modify: `rust-cli/src/ops/task.rs`
- Modify: `rust-cli/src/ops/claim.rs`
- Modify: `rust-cli/src/redis_bus.rs`
- Modify: `rust-cli/src/models.rs`
- Modify: `rust-cli/src/http.rs` (dashboard additions)
- Modify: `rust-cli/src/cli.rs`
- Test: `rust-cli/tests/orchestration_test.rs`

**Remaining P2 items:**
1. Validated task cards (replace opaque queue strings)
2. Durable resource-event notifications
3. Server-assisted reroute suggestions
4. Ack deadlines for high-risk resources
5. Cross-repo resource scopes
6. Dashboard: claims, tasks, pending ACKs
7. Server-side orchestrator summaries
8. Generalize codex_bridge into agent profiles

### Steps

- [ ] **Step 1: Add validated task cards**

  Replace `push_task(agent, task_string)` with `push_task(agent, TaskCard)` where:
  ```rust
  pub(crate) struct TaskCard {
      pub(crate) id: String,
      pub(crate) repo: Option<String>,
      pub(crate) paths: Vec<String>,
      pub(crate) priority: String,
      pub(crate) depends_on: Vec<String>,
      pub(crate) reply_to: Option<String>,
      pub(crate) tags: Vec<String>,
      pub(crate) status: TaskStatus, // pending, in_progress, done, failed
      pub(crate) body: String,
  }
  ```
  Store as JSON in Redis list. Backwards-compatible: bare strings deserialize as `TaskCard { body: string, .. }`.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 2: Add durable resource-event notifications**

  When a claim is created/renewed/released/resolved, post a notification to all agents subscribed to that `resource_id`, repo, or path prefix. Reuse the subscription infrastructure from Task 5.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 3: Add reroute suggestions for namespaced resources**

  When a claim conflict is detected for a namespaceable resource (cargo target, coverage dir, bench output), return a `suggested_namespace` field in the claim response instead of just blocking.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 4: Add cross-repo resource scopes**

  Extend claim keys from `bus:claim:{resource}` to `bus:claim:global:{resource}` for machine-wide paths. Add `scope: "repo" | "global"` to `ClaimOptions`. Agents in one repo can see global contention.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 5: Wire high-risk resource claims to auto-ack deadlines**

  Define a configurable list of high-risk resource patterns (`~/bin/*`, `~/.config/*`, `**/Cargo.lock`, service names). When a claim is created matching these patterns, auto-set `request_ack=true` and register an ack deadline using the infrastructure from Task 5. This fulfills TODO.md P2 "ack deadlines for high-risk resources."

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 6: Surface claims/tasks/ACKs in dashboard**

  Add sections to the HTTP `/dashboard` endpoint: active claims, pending tasks per agent, overdue ACKs. Add to the web/ dashboard UI.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 7: Add orchestrator summaries**

  `GET /summary?since=5m` returns a compact "what changed since last poll" rollup: new messages (count by topic), claim changes, presence changes, overdue acks.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 8: Generalize codex_bridge into agent profiles**

  Rename/refactor `codex_bridge.rs` into `agent_profiles.rs`. Define `AgentProfile { name, sync_format, finding_format, config_paths }`. Support Claude, Codex, Gemini profiles. The `codex-sync` command becomes `agent-sync --profile codex`.

  **Note:** If Task 9 (workspace split) starts before this step, move `codex_bridge.rs` as-is to `agent-bus-core` and defer the rename until this task completes.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 9: Commit**

  ```bash
  git commit -S -am "feat(orchestration): add task cards, resource events, reroute, and agent profiles"
  ```

---

## Task 8: P3 — Token efficiency

**Files:**
- Modify: `rust-cli/src/ops/inbox.rs`
- Modify: `rust-cli/src/token.rs`
- Modify: `rust-cli/src/output.rs`
- Modify: `rust-cli/src/cli.rs`
- Modify: `rust-cli/src/http.rs`
- Test: `rust-cli/tests/token_efficiency_test.rs`

**Remaining P3 items:**
1. `summarize-session` and `summarize-inbox` APIs
2. Shorten repeated tag prefixes in compact encodings
3. Excerpt mode for long findings
4. Benchmark token reduction across encodings
5. `compact-thread` / `summarize-thread` (implementation lives in Task 6 Step 4; this task consumes it for token optimization)

### Steps

- [ ] **Step 1: Add summarize-session and summarize-inbox**

  In `ops/inbox.rs`: `summarize_session(settings, session_id) -> SessionSummary` and `summarize_inbox(settings, agent) -> InboxSummary`. Both produce token-efficient rollups: message counts by topic, key findings, latest status per agent.

  CLI: `summarize-session`, `summarize-inbox`. HTTP: `GET /sessions/:id/summary`, `GET /inbox/:agent/summary`.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 2: Add tag prefix compression**

  In `output.rs`, for compact/minimal/toon encodings: detect repeated prefixes (e.g., `repo:`, `session:`) and emit a prefix table at the start, then use short references. Only when 3+ messages share prefixes.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 3: Add excerpt mode**

  Add `--excerpt` flag to `read`, `list_messages`, `check_inbox`. When set, truncate body to 200 chars and add `"excerpt": true, "full_id": "<message_id>"` so callers can fetch the full body on demand.

  Run: `cd rust-cli && cargo ab-test`
  Expected: PASS

- [ ] **Step 4: Add token benchmark harness**

  In `rust-cli/benches/token_benchmarks.rs`: generate realistic multi-agent session data (50 messages, mixed topics/schemas). Measure token count for JSON, compact, minimal, TOON, and MessagePack. Output comparison table.

  Run: `cd rust-cli && cargo bench token`
  Expected: benchmark results printed

- [ ] **Step 5: Commit**

  ```bash
  git commit -S -am "feat(tokens): add summarize APIs, tag compression, excerpt mode, and benchmarks"
  ```

---

## Task 9: Workspace split (Phase 3-4)

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/agent-bus-core/Cargo.toml`
- Create: `crates/agent-bus-core/src/lib.rs`
- Create: `crates/agent-bus-cli/Cargo.toml`
- Create: `crates/agent-bus-cli/src/main.rs`
- Create: `crates/agent-bus-http/Cargo.toml`
- Create: `crates/agent-bus-http/src/main.rs`
- Create: `crates/agent-bus-mcp/Cargo.toml`
- Create: `crates/agent-bus-mcp/src/main.rs`
- Modify: `rust-cli/` (becomes `crates/agent-bus-core/` or removed)
- Modify: `.cargo/config.toml` (update aliases for workspace)
- Modify: `build.ps1`, `scripts/build-deploy.ps1`, `scripts/bootstrap.ps1`
- Modify: `scripts/validate-agent-bus.ps1`, `scripts/setup-agent-hub-local.ps1`

**Context:** After Tasks 1-4, the transport modules are thin adapters over the shared ops layer. The split should be mechanical — move modules to the right crate, adjust `use` paths, update Cargo.toml dependencies.

### Steps

- [ ] **Step 1: Create workspace root Cargo.toml**

  Create top-level `Cargo.toml` with `[workspace]` and `members = ["crates/*"]`. Move both the `[profile]` sections AND the `[lints.rust]` / `[lints.clippy]` sections (28 lines) from `rust-cli/Cargo.toml` to workspace root using `[workspace.profile.*]` and `[workspace.lints.*]`. Each member crate uses `[lints] workspace = true`.

  Run: `cargo check`
  Expected: builds (with `rust-cli` still present as a workspace member)

- [ ] **Step 2: Create agent-bus-core crate**

  Move shared modules: `ops/`, `models.rs`, `settings.rs`, `redis_bus.rs`, `postgres_store.rs`, `channels.rs`, `validation.rs`, `token.rs`, `output.rs` (shared TOON/compact formatters), `codex_bridge.rs` (or `agent_profiles.rs` if Task 7 Step 8 completed), `journal.rs`. This crate has NO clap, NO axum, NO rmcp dependencies.

  **Important:** `monitor.rs` and `mcp_discovery.rs` are CLI-only — move them to `agent-bus-cli`, not core.

  Shared runtime infrastructure currently in `lib.rs` must also move to core:
  - `PG_WRITER` OnceLock and `pg_writer()` accessor
  - `PgWriter::spawn` initialization
  - `spawn_pg_health_monitor` setup
  - `maybe_announce_startup` helper (uses ops::post_message and ops::set_presence)
  - `MAIN_THREAD_STACK_BYTES` constant and the `main_entry_with_args` thread/runtime builder pattern

  Each surface binary will call a core-provided `init_runtime()` → `init_pg()` → dispatch pattern.

  Run: `cargo check -p agent-bus-core`
  Expected: PASS

- [ ] **Step 3: Create agent-bus-cli crate**

  Move `cli.rs`, `commands.rs`, `server_mode.rs`, `monitor.rs`, `mcp_discovery.rs`, `main.rs`. Depends on `agent-bus-core`. Adds `clap` and `reqwest` (server-mode). CLI-specific output formatters (human/table) move here if split in Task 4 Step 5.

  The `main_entry()` function moves here, calling core's `init_runtime()` for PG/Redis setup before dispatch.

  Run: `cargo check -p agent-bus-cli`
  Expected: PASS

- [ ] **Step 4: Create agent-bus-http crate**

  Move `http.rs`. Depends on `agent-bus-core`. Adds `axum`, `tokio-stream`.

  Run: `cargo check -p agent-bus-http`
  Expected: PASS

- [ ] **Step 5: Create agent-bus-mcp crate**

  Move `mcp.rs`. Depends on `agent-bus-core`. Adds `rmcp`.

  Note: The `#![expect(clippy::multiple_crate_versions)]` annotation (rmcp/redis diamond) currently lives in `lib.rs`. After the split, it is only needed in `agent-bus-mcp`'s crate root, not in core. Audit and move lint annotations accordingly.

  Run: `cargo check -p agent-bus-mcp`
  Expected: PASS

- [ ] **Step 6: Migrate benchmarks and integration tests**

  Move `rust-cli/benches/bus_benchmarks.rs` and `rust-cli/benches/throughput.rs` to `crates/agent-bus-core/benches/` (they test core ops/token performance). Move `rust-cli/tests/channel_integration_test.rs` to `crates/agent-bus-core/tests/`. Move `rust-cli/tests/http_integration_test.rs` to `crates/agent-bus-http/tests/`. Update all `use agent_bus::` imports to match new crate names.

  Run: `cargo test --workspace`
  Expected: PASS

- [ ] **Step 7: Remove rust-cli/ (or keep as compatibility shim)**

  If keeping `rust-cli/` as a compatibility member: have it re-export the three binaries. Otherwise, delete and update all script references.

  Run: `cargo build --workspace`
  Expected: PASS — all three binaries produced

- [ ] **Step 8: Update build scripts**

  Update `build.ps1`, `.cargo/config.toml` aliases, `build-deploy.ps1`, `bootstrap.ps1`, `validate-agent-bus.ps1`, `setup-agent-hub-local.ps1` to target the new crate paths. Ensure `agent-bus.exe`, `agent-bus-http.exe`, `agent-bus-mcp.exe` are still the installed binary names.

  Run: `pwsh -NoLogo -NoProfile -File build.ps1 -FastRelease`
  Expected: PASS

- [ ] **Step 9: Run full test suite**

  ```bash
  cargo test --workspace
  cargo clippy --workspace --all-targets -- -D warnings
  cargo fmt --all --check
  ```
  Expected: all PASS

- [ ] **Step 10: Commit**

  ```bash
  git commit -S -am "refactor: split into agent-bus-core/cli/http/mcp workspace crates"
  ```

---

## Task 10: Validation matrix (Phase 5, P4)

**Files:**
- Create: `crates/agent-bus-core/tests/ops_parity_test.rs`
- Create: `crates/agent-bus-core/tests/pg_regression_test.rs`
- Modify: `rust-cli/tests/` or `crates/*/tests/` (expand existing tests)

**Remaining P4 items:**
1. CLI server-mode tests via `AGENT_BUS_SERVER_URL`
2. End-to-end MCP tool tests
3. HTTP coverage for /dashboard, /tasks, /token-count, /compact-context
4. Mixed multi-repo session tests (scoping prevents bleed-through)
5. PG jsonb regression coverage
6. CLI/HTTP parity tests for read-direct, compact-context

### Steps

- [ ] **Step 1: Add CLI/HTTP/MCP parity tests**

  For each core verb (send, ack, presence, read, claim, channel-post, check-inbox, compact-context): send the same input through all three transports and assert identical semantic results. Use `ops` layer directly for the expected value.

  Run: `cargo test parity -- --ignored`
  Expected: PASS

- [ ] **Step 2: Add CLI server-mode tests**

  Start `agent-bus-http` in a background task. Run CLI commands with `AGENT_BUS_SERVER_URL=http://localhost:<port>`. Assert results match direct-Redis execution.

  Run: `cargo test server_mode -- --ignored`
  Expected: PASS

- [ ] **Step 3: Add MCP end-to-end tests**

  For each MCP tool: construct a `CallToolRequestParams`, invoke via `AgentBusMcpServer::call_tool`, assert success and correct result shape.

  Run: `cargo test mcp_e2e -- --ignored`
  Expected: PASS

- [ ] **Step 4: Add PG jsonb regression tests**

  Send messages with complex metadata (`nested objects, arrays, null values`). Read back via PG-backed paths. Assert metadata round-trips correctly. Test `compact-context` with jsonb metadata.

  Run: `cargo test pg_jsonb -- --ignored`
  Expected: PASS

- [ ] **Step 5: Add multi-repo scoping tests**

  Two agents post to `repo:alpha` and `repo:beta`. Assert `check_inbox` with `repo:alpha` scope returns only alpha messages. Assert scoped cursors advance independently.

  Run: `cargo test multi_repo -- --ignored`
  Expected: PASS

- [ ] **Step 6: Add HTTP endpoint coverage**

  Tests for `GET /dashboard`, `POST /tasks/:agent`, `GET /tasks/:agent`, `POST /token-count`, `POST /compact-context`.

  Run: `cargo test http_endpoints -- --ignored`
  Expected: PASS

- [ ] **Step 7: Commit**

  ```bash
  git commit -S -am "test: add parity, server-mode, MCP e2e, PG regression, and scoping tests"
  ```

---

## Task 11: Documentation and packaging (Phase 6, P5-P6)

**Files:**
- Modify: `README.md`
- Modify: `CLAUDE.md`
- Modify: `AGENTS.md`
- Modify: `AGENT_COMMUNICATIONS.md`
- Modify: `MCP_CONFIGURATION.md`
- Modify: `TODO.md`
- Modify: `agents.TODO.md`

**Remaining P5 items:**
1. Document qmd usage and troubleshooting
2. Package reproducible bootstrap artifacts

**Remaining P6 items:**
1. Execute remaining structural steps (done by Task 9)
2. Extract typed service layer (done by Tasks 1-4)

### Steps

- [ ] **Step 1: Update README.md with workspace crate map**

  Add a "Crate Layout" section explaining `agent-bus-core`, `agent-bus-cli`, `agent-bus-http`, `agent-bus-mcp`. Document which binary to use when.

  Run: n/a (docs only)

- [ ] **Step 2: Update CLAUDE.md with new module table and build commands**

  Replace the module table with the workspace-based layout. Update `cargo` commands to use workspace syntax.

  Run: n/a (docs only)

- [ ] **Step 3: Update AGENT_COMMUNICATIONS.md with subscription/thread docs**

  Add sections for: explicit subscriptions, thread membership, ack deadlines, scoped inbox cursors.

  Run: n/a (docs only)

- [ ] **Step 4: Update MCP_CONFIGURATION.md with new tools**

  Add docs for `subscribe`, `unsubscribe`, `summarize_inbox`, `summarize_session` MCP tools.

  Run: n/a (docs only)

- [ ] **Step 5: Add brief qmd integration note**

  Add a short section to `README.md` or `docs/qmd-integration.md` covering how agents use `qmd` to search agent-bus docs. Keep it brief — full qmd troubleshooting belongs in the qmd repo, not here. This fulfills TODO.md P5 "Document qmd usage" at the scope appropriate for this repo.

  Run: n/a (docs only)

- [ ] **Step 6: Package reproducible bootstrap**

  Update `scripts/bootstrap.ps1` to work on clean machines: download Redis/PG if missing, create config dir, build from source. Remove local path assumptions.

  Run: `pwsh -NoLogo -NoProfile -File scripts/bootstrap.ps1 -DryRun`
  Expected: no errors

- [ ] **Step 7: Mark TODO.md items complete**

  Update `TODO.md` — mark all completed P0-P6 items. Update `agents.TODO.md` — mark all phases complete.

  Run: n/a (docs only)

- [ ] **Step 8: Commit**

  ```bash
  git commit -S -am "docs: update all project documentation for workspace layout and new features"
  ```

---

## Execution Order Summary

| Order | Task | Depends On | Est. Files | Priority |
|-------|------|------------|------------|----------|
| 1 | Task 1: ops/inbox+message | — | 10 | P0 |
| 2 | Task 2: ops/channel+claim | Task 1 | 8 | P0 |
| 3 | Task 3: ops/task+admin | Task 1 (parallel with Task 2) | 6 | P0 |
| 4 | Task 4: normalize transports | Tasks 1-3 | 4 | P0 |
| 5 | Task 5: P0 signaling | Task 4 | 9 | P0 |
| 6 | Task 6: P1 query model | Task 4 | 8 | P1 |
| 7 | Task 7: P2 orchestration | Tasks 5-6 | 8 | P2 |
| 8 | Task 8: P3 tokens | Task 4 | 6 | P2 |
| 9 | Task 9: workspace split | Task 4 | 15+ | P1 |
| 10 | Task 10: validation matrix | Task 9 | 6 | P1 |
| 11 | Task 11: docs & packaging | All | 8 | P2 |

**Parallelizable:** Tasks 2 and 3 can proceed in parallel after Task 1. Tasks 5, 6, 8, 9 can proceed in parallel after Task 4 completes. Tasks 5 and 6 feed into Task 7.

## Risk Mitigations

| Risk | Mitigation |
|------|------------|
| HTTP/MCP response drift during ops extraction | Parity tests (Task 10) catch drift early |
| Script breakage during workspace split | Keep binary names stable; update scripts atomically |
| Hidden coupling in commands.rs JSON shaping | Audit in Task 4 before splitting |
| PG fallback regressions | jsonb regression tests (Task 10 Step 4) |
| Test coverage gaps around notification cursors | Inbox ops tests (Task 1 Step 6) |
| output.rs split: http.rs depends on TOON formatters | Task 4 Step 5 explicitly splits output.rs; Task 9 places shared formatters in core |
| Shared runtime infra (PG_WRITER, Tokio, startup) during crate split | Task 9 Step 2 moves init infrastructure to core with explicit list |
| rmcp lint annotation misplacement after split | Task 9 Step 5 audits and moves crate-level lint suppression |
| codex_bridge rename ordering with parallel Task 7/9 | Task 7 Step 8 documents move-then-rename strategy |
| Benchmark/test breakage during workspace split | Task 9 Step 6 explicitly migrates benches and integration tests |
