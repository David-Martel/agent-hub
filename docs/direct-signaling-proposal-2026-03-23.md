# Direct Signaling Proposal (2026-03-23)

## Summary

The current `agent-bus` already has most of the raw transport needed for strong
multi-agent coordination:

- durable message history in Redis Streams
- low-latency fanout via Redis Pub/Sub
- per-agent HTTP SSE push
- explicit `request_ack`
- `thread_id`, `reply_to`, tags, direct channels, group channels, and claims

What it does **not** have is a durable, first-class notification model. As a
result, active agents still rely on manual polling (`read`, `list_messages`,
`check_inbox`) and human reminders to notice high-signal messages.

This proposal adds a robust direct signaling layer on top of the existing bus:

1. durable per-agent notification streams
2. explicit subscriptions over threads/tags/topics/resources
3. ack-aware reminder and escalation rules
4. transport adapters that push notifications into active agent sessions when
   possible, and replay missed notifications when not

The goal is to remove the human from routine message forwarding while keeping
the current stream-first architecture intact.

## Observed Patterns

### 1. The real conversation model is already subscription-shaped

From the March 23-24, 2026 WezTerm thread and the March 23, 2026
finance-warehouse traffic, agents consistently organize around:

- `repo:*` tags
- stable `thread_id` values
- direct recipient targeting (`to=codex`, `to=claude`)
- broad broadcasts for shared state (`to=all`)
- ownership and conflict messages
- blocking asks marked with `request_ack=true`

In practice, agents are already behaving as if they have subscriptions such as:

- "tell me anything in `repo:wezterm` for thread `wezterm-rust-optimization-20260324`"
- "tell me when someone messages `codex` directly"
- "tell me when a claimed resource overlaps my current lane"
- "tell me when a blocking ask to me has not been acknowledged"

The system should make those subscriptions explicit.

### 2. Manual polling is still part of the protocol

The startup prompt, MCP instructions, and operator docs still tell agents to
check the bus every 2-3 tool calls. That instruction appears in:

- startup status messages
- [AGENT_COMMUNICATIONS.md](C:\codedev\agent-bus\AGENT_COMMUNICATIONS.md)
- [rust-cli/src/settings.rs](C:\codedev\agent-bus\rust-cli\src\settings.rs)
- [rust-cli/src/mcp.rs](C:\codedev\agent-bus\rust-cli\src\mcp.rs)

This is the main failure mode the WezTerm thread exposed. The collaboration was
good, but liveness depended on both agents remembering to poll.

### 3. High-value messages are getting mixed with low-value noise

Across the sampled history:

- startup messages are frequent and repetitive
- broad status updates are useful but verbose
- direct blocking asks are rare and important
- conflict notices are urgent and time-sensitive
- ack replies are tiny but operationally critical

A notification layer should deliver:

- urgent direct asks immediately
- conflict and contested-claim events immediately
- ownership and status chatter only when subscribed
- digested or coalesced reminders for repeated updates

### 4. Current cursoring is too coarse

The current `check_inbox` model uses one cursor key per agent:

- `bus:cursor:<agent>`

That is better than replaying the entire stream, but still too coarse for
multi-repo, multi-thread work. The repo assessment already identified this gap:

- [docs/agent-bus-assessment-2026-03-22.md](C:\codedev\agent-bus\docs\agent-bus-assessment-2026-03-22.md)

One global cursor per agent is not a real subscription state model.

## Proposal

## 1. Add Durable Notification Streams

Keep the existing `agent_bus:messages` stream as the canonical message log.

Add a second layer of derived per-agent notification streams:

- `agent_bus:notify:<agent>`

Each notification is a compact envelope that references the canonical message:

```json
{
  "notification_id": "uuid",
  "agent": "codex",
  "message_id": "uuid",
  "stream_id": "1774313812682-0",
  "reason": "direct_request_ack",
  "priority": "urgent",
  "thread_id": "wezterm-rust-optimization-20260324",
  "tags": ["repo:wezterm", "conflict", "wezterm-watch"],
  "created_at": "2026-03-24T00:56:52.682178Z",
  "requires_ack": true,
  "ack_deadline_utc": "2026-03-24T01:01:52.682178Z"
}
```

This avoids duplicating full bodies while making delivery state durable.

### Why

- Pub/Sub alone is lossy for disconnected clients.
- SSE alone only helps active HTTP clients.
- polling the main message stream forces each client to rediscover what matters.

Per-agent notification streams make "what needs my attention now?" durable.

## 2. Introduce First-Class Subscriptions

Add subscription records:

- `agent_bus:subs:<agent>`

Each subscription should declare:

- `subscription_id`
- `delivery_mode`: `push`, `digest`, `reminder_only`
- match rules:
  - `recipient = <agent>`
  - `recipient = all`
  - `thread_id`
  - `repo`
  - `session`
  - `tag`
  - `topic`
  - `priority >= X`
  - `resource`
  - `request_ack_only`
- `expires_at`
- `coalesce_window_ms`

### Default subscriptions

Every agent should get these automatically:

1. direct messages where `to == agent`
2. any message with `request_ack=true` where `to == agent`
3. `ownership`, `conflict`, and `resolve_claim` messages that mention resources
   currently claimed by that agent
4. replies in threads the agent started or explicitly joined

### Optional subscriptions

Agents may add:

- repo subscriptions: `repo:wezterm`
- thread subscriptions: `wezterm-rust-optimization-20260324`
- group subscriptions: `group:review-http-rs`
- role subscriptions: `topic=finding AND severity>=high`

## 3. Promote Threads To Joinable Conversation Scopes

Today `thread_id` is metadata. In practice it is already the main unit of
collaboration.

Add explicit thread membership:

- `agent_bus:thread_members:<thread_id>`

Agents join a thread when they:

- send a message with that `thread_id`
- explicitly call `join_thread`
- subscribe to that thread

Then any message in that thread can notify members according to subscription
rules.

### Why

The WezTerm thread used `thread_id` correctly, but the system still depended on
manual reads. Thread membership turns "ongoing collaboration" into something the
bus can route automatically.

## 4. Add Ack Deadlines, Reminders, And Escalation

`request_ack=true` should create more than a boolean on the message.

Add a pending-ack record:

- `agent_bus:pending_ack:<agent>`

Fields:

- `message_id`
- `thread_id`
- `from`
- `to`
- `deadline_utc`
- `reminder_count`
- `escalate_to`

Behavior:

1. sender posts message with `request_ack=true`
2. router emits immediate notification to recipient
3. if no ack by deadline, emit reminder notification
4. if still no ack after `N` reminders, escalate to configured fallback:
   `all`, orchestrator, or original sender

### Why

The WezTerm thread shows exactly this need. Several blocking coordination
messages were important, but noticing them still depended on the recipient
polling at the right time.

## 5. Separate Notification Routing From Message Storage

On every message post:

1. write canonical message to `agent_bus:messages`
2. evaluate subscriptions and implicit routing rules
3. write zero or more notification envelopes to `agent_bus:notify:<agent>`
4. publish lightweight realtime hints to Pub/Sub for already-connected clients
5. optionally push directly to active SSE or MCP sessions

This preserves the current message model and adds notification fanout as a
derived layer.

## 6. Upgrade Transport Adapters

### HTTP / SSE

The existing SSE support should become notification-centric:

- `GET /events/:agent` should stream notification envelopes, not only raw
  message events
- on connect, server should replay missed notification backlog from
  `agent_bus:notify:<agent>` before switching to live mode
- heartbeats should include pending-ack counts and contested-claim counts

### MCP

For MCP transports that support server notifications or streamable HTTP:

- deliver notification envelopes as tool-independent server events
- expose replay on reconnect using notification cursor

For stdio-only MCP clients:

- keep `check_inbox`, but back it by `agent_bus:notify:<agent>` rather than the
  full message stream
- rename the operational guidance from "poll the bus" to "drain your
  notifications"

### CLI / terminal agents

Add a dedicated command:

```powershell
agent-bus watch-notify --agent codex
```

This should:

- read replayable notification backlog
- subscribe to live hints
- optionally beep / toast
- optionally print a one-line prompt suitable for the active terminal

The existing [scripts/watch-agent-bus.ps1](C:\codedev\agent-bus\scripts\watch-agent-bus.ps1)
becomes the operator wrapper for this new path.

## 7. Make Claims And Conflicts Auto-Notifying

Claims already exist, but conflicts still surface late and conversationally.

Add resource watchers:

- when an agent claims `path/*`, auto-subscribe that agent to claim activity for
  overlapping resources
- when a second claim overlaps, emit a conflict notification immediately to:
  - current owner
  - challenger
  - orchestrator if configured

### Why

In the WezTerm thread, conflict handling worked, but only after agents noticed
the overlap and wrote manual messages. The bus should do that automatically.

## 8. Add Coalescing To Reduce Noise

Not every update deserves an interrupt.

Notification reasons should have routing classes:

- `interrupt_now`
  - direct message to me
  - `request_ack=true`
  - contested claim
  - escalation
- `show_soon`
  - same-thread update
  - claimed-resource activity
  - status from a subscribed repo/session
- `digest`
  - repeated startup
  - periodic status chatter
  - repeated "build still running" updates

Coalescing rule example:

- if 5 status messages arrive for the same thread within 60 seconds, emit one
  digest notification with message references instead of 5 interrupts

## Protocol Additions

## New Redis Keys

- `agent_bus:notify:<agent>`
- `agent_bus:subs:<agent>`
- `agent_bus:thread_members:<thread_id>`
- `agent_bus:pending_ack:<agent>`
- `agent_bus:notify_cursor:<agent>:<subscription_id>`

## New MCP / HTTP / CLI Surfaces

- `subscribe`
- `unsubscribe`
- `list_subscriptions`
- `watch_notifications`
- `read_notifications`
- `ack_notification`
- `join_thread`
- `leave_thread`
- `pending_acks`

### Backward compatibility

No existing message fields need to be removed.

`request_ack`, `thread_id`, `reply_to`, tags, and channels remain valid. The new
notification layer derives behavior from them.

## Recommended Delivery Rules

### Implicit notification rules

Create a notification when any of the following is true:

1. `to == agent`
2. `to == all` and agent subscribed to matching repo/thread/topic/tag
3. message mentions a resource the agent owns or watches
4. `reply_to` references a message sent by the agent
5. `request_ack == true` and `to == agent`

### Recommended reasons

- `direct_message`
- `direct_request_ack`
- `thread_update`
- `resource_conflict`
- `claim_resolved`
- `pending_ack_reminder`
- `escalation`
- `digest`

## Rationale

This design is a better fit for the observed conversations because:

1. agents are already coordinating by thread, repo, recipient, and ownership
2. the transport already has Streams, Pub/Sub, SSE, and cursor logic
3. the current failure is not storage or routing correctness, but missed
   attention
4. durable notifications solve the missed-attention problem without replacing
   the existing bus

It also matches the March 22, 2026 repo assessment:

- scope-aware cursors
- better repo/session/thread queries
- surfaced pending ACKs and claims
- less reliance on raw replay

## Rollout Plan

## Phase 1: Safe Increment

- add `agent_bus:notify:<agent>` fanout
- add `read_notifications`
- change `watch-agent-bus.ps1` to use notifications first
- update startup prompt and docs to stop telling agents to poll raw messages

Lowest risk, immediate operator value.

## Phase 2: Subscription Registry

- add explicit subscription CRUD
- add thread membership
- add scoped notification cursors
- back `check_inbox` with notifications

This removes the main manual-polling failure mode.

## Phase 3: Ack Automation

- add pending-ack records
- add reminder timers
- add escalation rules
- expose pending ACK dashboard widgets

This removes most "human middleman" reminder work.

## Phase 4: Transport Push Everywhere

- SSE replay + live notifications
- streamable MCP notifications where supported
- terminal-native watcher improvements
- optional desktop toast / bell adapters

## Success Metrics

The design is successful if:

- direct `request_ack` messages are noticed without human prompting
- contested claims surface within seconds
- active thread participants receive relevant updates without polling
- startup and low-signal chatter stop interrupting unrelated agents
- per-agent backlog replay works after disconnect or restart

## Concrete Recommendation

Implement Phase 1 and Phase 2 first.

Those two phases solve the actual operational problem exposed in the WezTerm
collaboration:

- the conversation quality is already high
- the missing piece is durable attention routing
- per-agent notification streams plus explicit subscriptions are the smallest
  change that fixes that problem cleanly

