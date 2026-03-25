# Resource Coordination Proposal

Date: 2026-03-24

## Problem

`agent-bus` already carries useful human coordination traffic, but resource
contention is still mediated by convention rather than by first-class bus
state. In practice, Codex and Claude are manually emulating a lease protocol:

- `RESOURCE_START` / `RESOURCE_UPDATE` / `RESOURCE_DONE` on direct threads
- `repo:<name>` tags plus stable `thread_id`
- namespaced build outputs instead of waiting
- explicit ack for installs, config edits, and shared runtime changes

That works in `wezterm`, but it is fragile. The failure modes are predictable:

- an agent misses the notification and starts conflicting work anyway
- a lock is announced in free text but not renewed or expired
- cross-repo machine-global resources are not visible from one repo view
- a build/install lane that could have been rerouted to a private namespace
  still blocks because the bus has no structured conflict response

## Evidence

Observed coordination patterns:

- [`C:\Users\david\wezterm\RESOURCE_COORDINATION.md`](C:\Users\david\wezterm\RESOURCE_COORDINATION.md)
  defines three useful resource classes already in live use:
  cooperative, shared-namespaced, and exclusive.
- Recent direct `codex` <-> `claude` traffic uses structured free-text payloads
  with `resource=`, `mode=`, `cmd=`, `namespace=`, `eta=`, `status=`, and
  `follow_up=`.
- Recent `repo:wezterm` bus traffic shows cross-repo and machine-global work:
  `~/bin` installs, shared config edits, repo commit/PR ownership, and
  documentation work spanning both `repo:wezterm` and `repo:agent-bus`.

The important conclusion is that the protocol is already socially correct. The
missing piece is durable resource state plus targeted notifications.

## Proposal

Add a first-class resource coordination subsystem to `agent-bus` built on top
of the existing message stream and notification infrastructure.

Core model:

- `resource_id`: stable logical name for the resource being coordinated
- `scope`: where the resource lives
- `mode`: `shared`, `shared_namespaced`, `exclusive`
- `holder`: agent holding the lease
- `thread_id`: coordination thread for the work
- `repo_scopes`: one or more repos affected by the lease
- `namespace`: required for namespaced shared resources
- `lease_ttl_seconds`: expiry window
- `ack_required`: whether another agent must confirm before acquisition

## Resource Scope Types

Resource scope should not be repo-only. It needs to model machine-global
contention directly.

Recommended scope kinds:

- `repo_path`: path inside one repo
- `cross_repo_path`: path set spanning multiple repos
- `user_config`: files under `%USERPROFILE%` or equivalent live config roots
- `service`: Windows service or daemon instance
- `install_target`: shared deployed binary path such as `~/bin/*`
- `artifact_root`: cargo target dir, coverage dir, benchmark output root
- `cache`: shared cache service or mutable cache state
- `external`: named external dependency like Redis, PostgreSQL, or WT settings

Examples:

- `resource_id=repo:wezterm:path:C:/Users/david/wezterm/Cargo.lock`
- `resource_id=install:C:/Users/david/bin/agent-bus-http.exe`
- `resource_id=service:AgentHub`
- `resource_id=artifact:T:/RustCache/cargo-target`
- `resource_id=config:C:/Users/david/.wezterm.lua`

## New Bus Operations

Add explicit resource operations:

- `resource_acquire`
- `resource_renew`
- `resource_release`
- `resource_wait`
- `resource_subscribe`
- `resource_list`
- `resource_conflicts`

These should coexist with the normal message bus, not replace it.

## Notification Model

Resource events should emit durable notification envelopes to subscribed agents.

Recommended event types:

- `RESOURCE_ACQUIRED`
- `RESOURCE_RENEWED`
- `RESOURCE_RELEASED`
- `RESOURCE_CONFLICT`
- `RESOURCE_ACK_REQUIRED`
- `RESOURCE_ACK_TIMEOUT`
- `RESOURCE_REROUTE_AVAILABLE`

Subscription scopes:

- by `resource_id`
- by repo
- by `thread_id`
- by scope kind
- by path prefix

This is where the existing direct-signaling work helps: resource events should
land in the same replayable per-agent notification streams as direct messages.

## Server Behavior

The server should do more than store leases.

When an agent requests a resource:

1. Normalize the resource into a canonical scope.
2. Detect conflicts against active leases.
3. If mode is `shared_namespaced`, suggest a private namespace when possible.
4. If mode is `exclusive` and conflict-free, issue a lease with TTL.
5. If ack is required, emit `RESOURCE_ACK_REQUIRED` to interested agents.
6. If no ack arrives by deadline, emit reminder or downgrade to reroute advice.

Important behavior:

- Lease expiry must be automatic if the holder disappears or stops renewing.
- `thread_id` should bind related resource actions into one resumable stream.
- Cross-repo conflicts should be surfaced even when the repos differ, if the
  underlying path/service/install target is the same.

## Namespaced Resource Assistance

The bus should help agents avoid waiting when isolation is possible.

For `shared_namespaced` resources, return a suggested namespace:

- build: `T:/RustCache/cargo-target/<agent>/<task>`
- coverage: `.cache/<agent>/<task>/coverage`
- bench output: `.cache/<agent>/<task>/bench`
- temp install validation: `%TEMP%/<agent>/<task>`

That turns today’s free-text rule into a server-assisted fallback.

## Commands And HTTP Endpoints

Suggested CLI:

```powershell
agent-bus resource-acquire --agent codex --resource install:C:\Users\david\bin\agent-bus-http.exe --mode exclusive --thread-id bus-http-rollout --repo agent-bus --ack-required
agent-bus resource-release --agent codex --resource install:C:\Users\david\bin\agent-bus-http.exe --status ok
agent-bus resource-list --repo wezterm
agent-bus resource-conflicts --agent claude --repo wezterm
agent-bus resource-subscribe --agent claude --repo wezterm --scope-kind install_target
```

Suggested HTTP:

- `POST /resources/acquire`
- `POST /resources/{resource_id}/renew`
- `POST /resources/{resource_id}/release`
- `GET /resources`
- `GET /resources/conflicts`
- `GET /resources/events/{agent}`

## Why This Is Better

This proposal keeps the good parts of the current workflow:

- narrow repo/thread scoping
- human-readable coordination
- direct-channel pairwise handoffs
- isolation over waiting

But it removes the main weaknesses:

- no more invisible free-text locks
- no orphaned resource claims with no expiry
- no dependence on periodic manual bus polling to notice a collision
- no repo-only blind spot for shared machine resources

## Recommended MVP

Implement in this order:

1. Resource lease records in Redis with TTL and replayable notifications.
2. `resource_acquire` / `resource_release` / `resource_list`.
3. Scope normalization for install targets, services, user config, and artifact
   roots.
4. `shared_namespaced` conflict responses with suggested private namespaces.
5. Ack deadlines plus reminder notifications for exclusive/high-risk resources.
6. PostgreSQL mirroring and indexed resource history for audits/dashboard views.

## Relationship To Existing TODO

This proposal depends on and extends existing work already on the roadmap:

- explicit subscriptions
- repo/session/thread-scoped cursors
- ack reminders/escalation
- dashboard visibility for claims and contested ownership

It should be tracked as a dedicated resource-coordination slice rather than
hidden inside generic claims or task queue work.
