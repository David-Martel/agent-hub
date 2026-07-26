# Fleet Reconciliation And History Plan — 2026-07-25

## Decision

Use one authoritative durable hub on `asuspro13`. Treat `dtm-p1gen7`,
`spark-0060`, and `spark-3066` as clients unless a documented disaster-recovery
exercise explicitly promotes another machine.

This preserves one PostgreSQL audit trail and one Redis coordination stream.
Running un-federated local hubs on clients creates split history and false
presence. A relay is only appropriate when a machine also runs a deliberate
local hub.

Client configuration must use stable hostnames:

- same-machine access: `http://localhost:8400`
- `spark-0060`: `http://asuspro13-p2p-spark-0060:8400`
- `spark-3066`: `http://asuspro13-p2p-spark-3066:8400`
- other remote clients: the stable routed hostname for `asuspro13`, never a
  private numeric address embedded in config

## Evidence Snapshot

The inventory was read-only. All four machines were reachable on 2026-07-25.
Remote repositories were clean; transient GitHub runner checkouts are not
deployment sources.

| Machine | Repository state at inventory | Installed runtime | Service/topology finding | Reconciliation |
|---|---|---|---|---|
| `dtm-p1gen7` | Local main initially matched `origin/main`; PR work was isolated and later merged | `0.5.0-5-gc0411bc` | NSSM `AgentHub` healthy on `0.0.0.0:8400`; historical Redis timeout and PostgreSQL broken-pipe logs | Rebuild/install from merged main; keep as a local development fallback, not a second fleet authority |
| `asuspro13` | Clean main at the then-current `18a3b62`; no extra worktrees | Installed plain `0.5.0`; newer target binaries existed but were not installed | Authoritative Redis/PostgreSQL hub healthy on `0.0.0.0:8400`; user unit incorrectly launched generic `agent-bus serve`; relay to the workstation recorded reachability flaps | Fast-forward, back up durable state, install exact build, replace unit with dedicated `agent-bus-http`, then validate remote clients |
| `spark-0060` | Clean main, one commit behind the then-current remote | `v0.5.0-15-g029e6e3` | No local hub, but an active relay repeatedly targeted missing `127.0.0.1:8400`; central route healthy; config mode `664` | Fast-forward/install client binaries, set config mode `600`, disable the invalid relay |
| `spark-3066` | Canonical clean clone two commits behind; duplicate clean clone seven behind | Plain `0.5.0` built from the older duplicate clone | Isolated local `agenthub.service` active while clients bypassed it for the healthy central route; relay disabled | Fast-forward/install from the canonical clone, disable the isolated hub, retain the old clone only as a clearly non-authoritative archive until separately approved for removal |

The installed semantic version alone was insufficient: all binaries advertised
`0.5.0` while their hashes and Git revisions differed. Exact build provenance
is therefore an operational requirement, separate from protocol compatibility.

## Repository And Pull Request Reconciliation

PR #44 updated the Cargo dependency group. It originally failed for two
independent reasons:

1. `rmcp` 2.x replaced the old `Content` model with `ContentBlock`.
2. The Windows self-hosted Docker job forced `bash`, causing a Windows
   temporary-script path to be interpreted as a Unix path.

Both defects were fixed on the PR branch. The full 12-job workflow passed,
including Windows build/unit tests, x64 and arm64 Docker builds, integration
tests, release builds, and CLI/HTTP smoke. The PR had no review comments or
unresolved threads and was squash-merged as `5cb9cc3`.

Five local `worktree-agent-*` branches remain as recoverable references. Each
is fully merged into main and no corresponding linked worktree remains. Delete
the branch references only after the follow-up fleet PR is merged and the final
worktree inventory is clean.

## Implemented Reliability Work

- Corrected the shared workstation coordination guide: client commands use
  `agent-bus.exe`; `agent-bus-http.exe` and `agent-bus-mcp.exe` are dedicated,
  blocking server processes. Accidentally using the HTTP binary as a client can
  start a competing listener on `localhost:8400`.
- Reconfigured the local Codex MCP entry from startup-time HTTP to the
  dedicated stdio binary, retaining an automatic backup of `config.toml`.
- Added exact Git build provenance to health and `negotiate` responses while
  retaining the semantic version in MCP initialization. This is additive and
  does not alter message, presence, or storage schemas.
- Added a deterministic stdio MCP handshake/tool-list smoke and stricter client
  config validation.
- Added a native systemd user-service template and installer path for the
  dedicated HTTP server.

## No-Loss Deployment Runbook

Run each gate in order. Stop on any mismatch; do not reset or overwrite a dirty
checkout.

### 1. Source and durable-state preflight

1. Confirm the follow-up PR is merged and every canonical clone can
   `pull --ff-only`.
2. Confirm `git status --porcelain` is empty on every deployment clone.
3. On `asuspro13`, record service status, health, stream length, PostgreSQL
   message count, and presence count.
4. Create a timestamped `agent-bus backup` from the authoritative hub and run
   `agent-bus validate-backup` against it.
5. Preserve the current installed binaries and user unit. The installer already
   stores replaced binaries in a timestamped backup directory.

### 2. Authoritative hub

1. Dry-run `./install.sh --with-http-service`.
2. Build and install from the clean canonical clone.
3. Inspect the rendered unit and its environment file. Authentication material
   must remain outside the unit and repository.
4. Restart only `agent-bus-http.service`.
5. Require local `/health`, authenticated MCP initialize, `tools/list`, and
   `negotiate` to pass. Verify the reported build revision matches the deployed
   commit and the tool count is 17.
6. Verify pre-deploy stream/PostgreSQL counts have not decreased.

### 3. Client machines

For each client, fast-forward and run the installer dry-run before installing.
Then:

- `spark-0060`: set `~/.config/agent-bus/config.json` to mode `600`; verify the
  central route; disable `agent-bus-relay.service`.
- `spark-3066`: install only from
  `/home/damartel/dev/repos/agent-hub`; verify the central route; disable
  `agenthub.service` and `agent-bus-relay.service`. Do not delete the older
  clone in this deployment.
- `dtm-p1gen7`: deploy the three exact binaries through the checked-in
  PowerShell deploy path. Preserve NSSM environment/auth settings.

### 4. End-to-end proof

1. Set presence from every machine with machine identity in capabilities or
   metadata.
2. Send one uniquely tagged smoke message from each client to another client
   through the central hub.
3. Read and acknowledge each message from the intended recipient.
4. Confirm all message IDs appear in the central PostgreSQL journal and no
   client-local hub accepted them.
5. Re-run health after a short idle interval and inspect service journals for
   Redis reconnect loops, PostgreSQL write errors, dropped writes, bind
   conflicts, and relay retries.

Rollback uses the installer backup binaries and preserved unit, followed by the
same health/count checks. Never restore Redis or PostgreSQL over a live service.

## Fleet Identity And Configuration Contract

Additive metadata should include:

- `machine_id`: stable fleet name such as `asuspro13` or `spark-0060`
- `os`: `windows` or `ubuntu`
- `arch`: `x86_64` or `aarch64`
- `build_version`: exact binary provenance
- `client_kind`: `codex`, `claude`, `gemini`, `antigravity`, or custom

Do not make these fields required in existing messages or presence rows.
Existing clients must continue to deserialize and operate with defaults.
Introduce a versioned fleet manifest as data, not hard-coded routing logic.
The manifest declares the authoritative hub, per-machine route, expected
architecture, service role, and minimum protocol/tool capability.

## Conversation History Centralization

Agent-bus coordination history and LLM session history are different data
classes. Keep the coordination bus small and live; ingest session history into
a separate additive catalog.

Observed local sources were approximately:

- Codex: 6,278 session JSONL files, about 9.0 GB
- Claude: 720 session JSONL files, about 712 MB
- Antigravity brain directories: about 312 MB total
- Gemini/IDE workspace history: a distinct provider format requiring its own
  adapter

Raw history can contain secrets, private prompts, identifiable media pointers,
and generated artifacts. It must not be broadcast through agent-bus or uploaded
unfiltered.

### Catalog schema

Create a separately migrated PostgreSQL schema:

```text
schema_migrations(version, applied_at, checksum)
sources(source_id, machine_id, provider, source_kind, path_hash,
        parser_version, cursor_json, last_seen_at, status)
sessions(session_id, source_id, provider_session_id, repo, branch,
         started_at, ended_at, event_count, sanitized_sha256, policy_version)
events(event_id, session_id, provider_event_id, timestamp_utc, event_type,
       role, content_redacted, metadata_jsonb, source_offset, content_sha256)
artifacts(artifact_id, session_id, kind, local_uri, drive_file_id, sha256,
          byte_count, redaction_report_jsonb)
```

Use deterministic IDs, unique provider/source constraints, resumable cursors,
content hashes, and parser/policy versions. Provider adapters for Codex,
Claude, Antigravity, and Gemini remain independent. Re-ingestion must be
idempotent.

### Redaction and quarantine gates

1. Inventory paths and sizes without reading payloads.
2. Parse locally into a staging area.
3. Detect credentials, tokens, private keys, connection strings, cookies,
   identifiable payload paths, and binary/media content.
4. Quarantine uncertain records. Reports contain rule names and counts, never
   secret values.
5. Generate searchable redacted events and concise session summaries.
6. Validate source/event counts, hashes, referential integrity, and replay
   idempotency.

### Google Drive backup

An empty `Agent History Backups` folder was created at the connected Drive
root. No history payload was uploaded. The connected app created the folder,
but the separate local Google Workspace OAuth token was expired, so an
independent permissions listing could not be completed in this run. Verify the
owner and absence of inherited/public sharing before the first upload.

The approved target layout is:

```text
Agent History Backups/
  v1/
    <pseudonymous-machine>/
      <year>/
        <run-id>/
          manifest.json
          sessions.ndjson.zst
          events-*.ndjson.zst
          checksums.sha256
          redaction-report.json
```

Before publication:

1. Upload only sanitized, compressed shards and manifests.
2. Restrict access to `davidmartel07@gmail.com`; do not create link sharing.
3. Record Drive file IDs and hashes in `artifacts`.
4. Download one shard from each run and verify its checksum and restore parser.
5. Keep raw provider files local under normal workstation backup policy.

## Remaining Work

### Deployment evidence (2026-07-26)

- PR #45 and the follow-up provenance fix in PR #46 were squash-merged after
  all format, Clippy, unit, integration, release, Docker, Windows, audit,
  benchmark, MCP-config, and functional smoke jobs passed.
- All four canonical checkouts and installed CLI/HTTP binaries report
  `v0.5.0-20-gfd70c8d`. ASUS runs the dedicated `agent-bus-http` user service
  on `0.0.0.0:8400`; its PostgreSQL and Redis counts increased across restart,
  with zero dropped writes or PostgreSQL write errors.
- Pre-centralization custom-format PostgreSQL dumps, Redis RDB validation,
  installed binaries, unit/config snapshots, manifests, and checksums are
  preserved on `dtm-p1gen7`, `asuspro13`, and `spark-3066`. Spark-0060 has a
  matching non-data-bearing rollback snapshot.
- The three historical databases had no overlapping message IDs in the
  payload-free comparison (about 20,857 distinct IDs). They were not replayed,
  renumbered, or deleted. Future traffic is routed to ASUS; historical sources
  remain immutable inputs for the additive catalog.
- Tagged send/read/ack probes succeeded from `dtm-p1gen7`, `spark-0060`, and
  `spark-3066` through ASUS. Only after those acknowledgements were read back
  were Spark-0060's invalid relay, Spark-3066's isolated hub, and ASUS's
  temporary DTM relay disabled.
- The local Windows deploy exposed two additional gates: persisted maintenance
  state blocked the first SSE write, and an active MCP process locked the
  unversioned executable. Maintenance was resumed and SSE passed; future MCP
  sessions now use a hash-verified version-suffixed binary so no active agent
  had to be terminated.
- `config/fleet/agent-bus-fleet-v1.json` is the non-secret desired-state
  inventory. `scripts/test-agent-bus-fleet.ps1` is a read-only doctor for
  build, route, permission, storage, write-integrity, and service-role drift;
  its offline schema fixtures run in Windows CI.

### Concurrent and fleet builds

Live concurrent builds in `agent-bus` and `vigil-utils` showed that CargoTools
0.9.0 counts normal `sccache` compiler clients as multiple servers and can stop
the shared daemon while other builds are active. The workstation also has
`sccache` 0.15.0 and 0.16.0 on different PATH entries, and the global Cargo
wrapper can override a requested no-cache build. Agent-bus retries now use an
explicit Cargo `build.rustc-wrapper=""` override and never stop the shared
daemon. The workstation module load order, canonical binary version, and
telemetry wrapper still require repair outside this repository.

The fleet build design should use isolated target namespaces and the existing
GitHub runners: Windows/x64 on `dtm-p1gen7`, Linux/x64 on ASUS, and Linux/ARM64
on both Spark machines. The dispatcher must record commit provenance, target,
runner, cache version/path, cache health, test counts, and artifact hashes.
Concurrent local builds should be a supported case, not a reason to
consolidate or restart a shared cache server.

- The first catalog slice is implemented as an explicit-only, checksummed
  `agent_history` migration. It uses deterministic 32-byte identities,
  parser/policy provenance, source cursors, redacted locators, and a
  transaction-scoped advisory lock without modifying current bus tables or
  message-backup formats.
- Apply the catalog migration to the authoritative hub only after the merged
  binary passes a disposable-database migration/rollback drill. Do not ingest
  provider history until redaction, quarantine, resumable cursor, and distinct
  catalog backup/restore gates are implemented.
- The 2026-07-26 local drill passed against a named disposable PostgreSQL
  database: migration apply/reapply, fixed schema snapshot, checksum round-trip,
  bus-table separation, nullable artifact deduplication, read-only status, and
  CLI no-op migration were verified; the disposable database was then removed.
  CI repeats the contract test with a run-scoped database and cleanup trap.
- Implement four resumable provider adapters and redaction/quarantine policy.
- Add machine identity to presence metadata by default.
- Keep the versioned fleet manifest current in every deployment PR and run the
  read-only doctor before retiring routes or services.
- Add checksum/completeness validation to coordination backups.
- Split liveness from authenticated readiness without changing existing
  `/health` behavior.
- Implement token-file/multi-token rotation support.
- Finish the thin CLI surface and remaining cross-platform release gates in
  `TODO.md`.
