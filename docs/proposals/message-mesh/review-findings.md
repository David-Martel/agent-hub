# Root integration review checkpoint

Owner: codex p1_hub_runtime_audit. Status: SOURCE_ONLY / NOT_COMPILED /
NOT_TESTED / NOT_DEPLOYED. Stopped at the user's usage-limit checkpoint.
No source edits, Cargo, Docker, backend tests or live calls were performed in
this review. The four current source hashes in MANIFEST.json bind this review
checkpoint only; they do not freeze or qualify the complete active assembly.

Reviewed source root:
../assembled-candidate/source/crates/agent-bus-http/src/

## Blocking finding

**P1: canonical notification receipts are discarded before recipient SSE.**
http.rs:1283-1326, especially the call to notification_from_message at line
1318, reconstructs a notification solely from event.message. It does not
consume the canonical event.notifications array published by mesh projection.
notification_from_message at lines 1220-1244 always sets
notification_stream_id=None (line 1242). notification_event at lines 1204-1217
adds the SSE event ID only when that stream ID exists.

Scenario: a mesh message commits its notification and APPLIED receipt with a
stable receiver-local notification stream ID, then publishes it. A connected
recipient receives the reconstructed notification without the committed SSE
cursor, so reconnect/resume cannot advance using that event's durable cursor.
The source records enough information, but this bridge throws it away.

Required fix: consume and validate event.notifications when the field is
present, including a valid empty array. Retain reconstruction only for legacy
events where the field is absent; malformed canonical arrays must not silently
fall back. Preserve the canonical IDs, stream IDs, agent/message binding and
notification fields. Keep one fanout path to avoid direct-plus-pubsub duplicate
delivery. Add a regression proving the committed stream ID reaches SSE and an
explicit empty array produces no synthesized notification. Re-review the
successor and run isolated QA before closing this finding.

## Confirmed source checks

- http.rs:3933-3955 applies the global client bearer middleware before merging
  the peer router. Peer replication therefore has its own authentication path
  and is not accidentally gated by the global client token.
- message_mesh.rs:431-486 authenticates configured peer headers before body
  collection, rejects stopped/maintenance ingress, acquires a bounded ingress
  permit without waiting, caps collection to MAX_BODY_BYTES with a five-second
  deadline, decodes JSON, checks stop/maintenance again, and invokes receive.
- peer_sync.rs:596-642 rejects duplicate identity/auth headers, binds a
  configured peer credential and compares bounded token bytes. receive at
  lines 698-725 binds authenticated source, local destination and scope before
  atomic store import. A successful ACK is created only after the store call
  and exact accepted-plus-duplicate count validation.
- http.rs:3770-3789 binds the listener before admitting mesh tasks. At
  lines 3958-3977 the graceful-shutdown signal sets the mesh watch immediately,
  before waiting for HTTP drain, and the owner explicitly awaits mesh shutdown
  afterward while preserving the first serving error.
- message_mesh.rs:161-171 joins the owned JoinSet on normal shutdown. PG
  adapters close/abort and join owned drivers after bounded operations;
  projection_once closes its owned Redis driver and PG store on normal paths.
  A stop request may still wait for the current bounded database/exchange
  operation; it does not prove instantaneous cancellation.
- http.rs:260-280 maps typed CaptureFailure to non-2xx statuses: invalid 400,
  collision 409, capacity 507 and pending/uncertain/unavailable 503. It preserves
  structured kind and original message IDs. There is no mesh pending 202.
  MCP dispatch preserves typed capture details in JSON-RPC error.data.
- The earlier canonical configuration-name defect was corrected to
  AGENT_BUS_MESH_JSON. Malformed or non-Unicode configured values retain an
  invalid mesh sentinel plus configuration error. require_claim_authority
  checks configuration errors before its pure global mesh parser.
- The designated-local-authority route requires explicit local_messages,
  mesh presence and successful require_claim_authority before bypassing remote
  selection/journal work. A nonauthority retains remote/pending behavior.
  This is source review only; authored regressions have not run.
- The obsolete transport MeshSettings wrapper is absent. Test-only transport
  helpers are cfg(test). No claim of compilation or runtime admission follows
  from these checks.

## Managed-process and operational boundaries

The existing notification bridge at http.rs:1283-1329 runs an infinite
spawn_blocking loop with sleeps and blocking pubsub.get_message. It has no
shutdown owner or join path. This predates the new mesh owner and remains a
separate whole-process termination risk: closing the HTTP listener and joining
mesh tasks alone do not prove that the process exited. An owning supervisor
must verify actual process termination; do not attribute a lingering process
to SSE without lifecycle evidence. A future owned/cancellable bridge change
needs its own regression and shutdown validation.

Peer credential files are loaded when a worker is admitted/recreated. The
successful inner push loop does not periodically reload them. Rotation or
revocation currently requires a worker/process restart unless a reviewed
reload policy is added. No token contents were read or printed by this review.

Next owner: Root. Preserve the frozen input candidates and this review packet;
fix the canonical notification path in a successor, then complete independent
review and the owned isolated QA gates. Mesh remains uncompiled and undeployed.
