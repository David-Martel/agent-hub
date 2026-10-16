# Disposable mesh QA handoff

Status: PLAN / SOURCE-ONLY. No Cargo, Docker, backend, build, install or deployment
was executed in this lane. Root owns the assembled source and all execution.
The assembled source was active during this review and has not been frozen or
hashed by this lane. Do not start until Root releases the shared warm directories.

## Admission and exact inputs

1. Root completes source assembly, review and formatter changes, then freezes a
   complete plain JSON mapping of relative source paths to SHA256. Include all
   files mounted at `/source`, including Cargo.lock and maintained test scripts;
   exclude `.git`, caches, credentials and local state. Record the mapping's hash.
   `base-source-manifest.json` is a historical input, not the final assembly.
2. Bind `required-tests.json` and this controller's manifest. It contains 62
   authored expectations from three frozen contributors: core 34/23 backend,
   transport 16/0 backend, capture 12/5 backend. These are source counts only.
   Capture includes two `outbox_hub::mesh_migration_tests` tests in addition to
   `mesh_capture::tests`. Root's later integration tests must also be selected
   and may be added to a new expectation revision. Never edit a prior failure.
3. Release the current hook/container owner first. Reuse the three existing
   owned RW cache directories at their existing in-container paths:

       /output/focused-core/cargo-home
       /output/focused-core/target
       /output/focused-core/sccache   (only when an admitted wrapper is used)

   Host root is `health-deadline-fix-plan/container-qa/resource-retry/qa-output`.
   Do not copy, delete, reset, recursively inventory or concurrently compile in
   these directories. Cargo fingerprints decide whether old artifacts apply.
   Check only Cargo-home top-level auth/config files and the existing exact-lock
   seed receipt; credentials/config files there are a HOLD. Do not mount host HOME.
4. Pinned QA image is `docker.io/library/rust@sha256:b36c246742b4d323472588f789601be34eb4af053255e2af53969078ee8dcec7`.
   Prior tool census reports Rust 1.99.0 and Bookworm GLIBC 2.36; this does not
   establish current image/tool presence or vigil1 GLIBC compatibility. Root
   verifies the actual created image ID, Rust/Cargo versions, Linux Docker CLI,
   Python and socket access before any fixture starts. Missing tools are HOLD;
   no ad hoc install/download in the test run. A checksum-bound Linux Docker CLI
   can be mounted read-only if the pinned image lacks it. Do not mount the
   Windows docker.exe into Linux. The optional sccache binary needs its own
   checked hash/version, private server port and stop/join receipt. No nextest
   substitution: the harness and exact-name validator expect ordinary libtest.

## Container and environment route

Reuse Root's already reviewed create/inspect/start/settle/remove controller.
Use source and controller RO, fresh output RW, the three admitted warm mounts RW,
owned Linux tmpfs `/owned-tmp` (UID/GID1000, mode0700), read-only rootfs,
UID1000, bounded CPU/memory/PIDs, cap-drop ALL, no-new-privileges, pull=never.
Keep the runner network `none`. Use a Linux Unix Docker socket and only its
necessary supplementary group; do not use host-network, privileged mode or a
remote Docker daemon. The socket conveys daemon authority despite a RO source
mount: admit only the reviewed snapshot/scripts and do not claim a security
sandbox against malicious tests. Source RO protects source bytes, not Docker.

Capture the runner's full 64-hex ID from create. Inspect it before start and pass
that exact ID to the harness. The fixture containers join its loopback via
`--network container:<id>`; no host service ports are published. A network-none
runner still has loopback, so HTTP, Redis and PG communicate there. Using native
host publishing from inside the runner would point localhost at the wrong host.

Clear the complete child environment, then allow only tool/runtime variables:
PATH, private HOME/LOCALAPPDATA/TMPDIR/TEMP/TMP, CARGO_HOME, RUSTUP_HOME,
CARGO_TARGET_DIR, CARGO_NET_OFFLINE=true, CARGO_BUILD_JOBS=2,
CARGO_PROFILE_DEV_CODEGEN_UNITS=16, CARGO_PROFILE_TEST_CODEGEN_UNITS=16,
CARGO_INCREMENTAL=0, RUSTUP_AUTO_INSTALL=0, RUSTUP_NO_UPDATE_CHECK=1,
DOCKER_HOST=unix:///var/run/docker.sock, private DOCKER_CONFIG containing `{}`,
and an optional admitted RUSTC_WRAPPER/SCCACHE_DIR/SCCACHE_SERVER_PORT.
Do not forward cloud/GitHub/agent tokens, SSH agents, proxy variables or user
config paths. Before service-free tests set AGENT_BUS_CONFIG to private `{}`,
AGENT_BUS_STARTUP_ENABLED=false, Redis `redis://localhost:1/0`, empty database
and server URLs. The maintained harness removes all AGENT_BUS and proxy input,
except an explicitly supplied build revision, and creates fresh test values.
Its integration mode preserves Unix HOME, so the outer private HOME matters.

## Exact command and evidence sequence

Run inside the admitted runner from `/source`, preserving stdout/stderr and exit
code separately for each phase. Stop after the first failing phase. Use raw logs,
not RTK summaries, for list/output comparison. All paths below are container
paths; actual host execution belongs to Root.

1. `qa-contract.py source-check` against Root's final manifest and
   `required-tests.json`, supplying their exact hashes and a fresh receipt path.
2. `cargo fmt --all --check`.
3. `cargo test --workspace --lib --bins --locked --offline -- --list`.
   Save stdout+stderr as `unit-list.log`. `qa-contract.py list-check --kind unit`
   must find every required service-free/owned-loopback name exactly once.
   In particular HTTP transport is a **binary** target, not a library target.
4. `cargo test --workspace --lib --bins --locked --offline -- --test-threads=4`.
   Preserve the raw output and exit code; `qa-contract.py result-check --kind unit`
   requires every named new unit/loopback test to pass once. Ignored backend
   tests remain ignored here; they are required in phase 7. This includes the
   real A-B-C-A loopback worker test and joined stalled-PG-startup test.
5. `cargo clippy --workspace --all-targets --locked --offline -- -D warnings`.
6. `cargo test --workspace --tests --locked --offline -- --ignored --list`.
   Save `backend-list.log`; `list-check --kind backend` checks every named new
   backend. Derive `COMPILED_BACKEND_COUNT` from its JSON receipt, not source
   attributes, CI comments or old receipts. Current CI floor is 85, harness
   default is 73; use max(85, actual compiled count) for the invocation below.
   No claim is made here about the final actual workspace count.
7. Maintained all-workspace fixture invocation:

       python3 -B scripts/ci/isolated-services.py integration \
         --network-container "$QA_RUNNER_ID" \
         --minimum-backend-tests "$COMPILED_BACKEND_COUNT"

   `QA_RUNNER_ID` is the captured/inspected ID, not a user hostname or a name
   lookup. The count must already be >=85. The harness builds agent-bus-http,
   starts and verifies its owned HTTP child, then runs exactly:

       cargo test --workspace --tests --no-fail-fast -- --ignored --test-threads=1

   Offline mode persists through the harness. The preceding --locked listings
   bind the same read-only lock/source. The maintained command itself has no
   --locked flag; do not conceal that difference. Root must check source/lock
   after the run. No new lock/dependency write can succeed in RO `/source`.
   `result-check --kind backend` must see the entire compiled ignored pass set,
   not only the 28 new expectations, exactly once. Summary pass counts must
   equal the compiled selection and no selected backend may be ignored.
8. Run maintained history separately in a fresh owned PG fixture, after integration:

       python3 -B scripts/ci/isolated-services.py history \
         --network-container "$QA_RUNNER_ID"

   Do not reuse the integration DB for history. History uses a different prefix
   and is not a substitute for the mesh fixtures' required HTTP identity.
9. Source-check again. Any byte/inventory drift fails QA. Check cleanup below
   even when a phase fails. Artifact smoke, minimal CLI feature check, packaging,
   target-specific build compatibility, Windows ACL qualification, fleet outage
   and deployment acceptance remain separate Root gates.

Controller syntax for each list/result phase (repeat source/hash arguments):

    python3 -I -B /controller/qa-contract.py result-check \
      --source /source --source-manifest /controller/assembled-source-manifest.json \
      --manifest-sha256 "$SOURCE_MANIFEST_SHA" \
      --contract /controller/required-tests.json --contract-sha256 "$CONTRACT_SHA" \
      --kind backend --listing /meshqa/backend-list.log \
      --output-log /meshqa/backend-run.log --exit-code "$ACTUAL_EXIT" \
      --receipt /meshqa/backend-result.json

`list-check` omits output-log/exit-code; `source-check` also omits kind/listing.
Receipts use exclusive creation and cannot overwrite old failures. Controller
output alone never proves process settlement or cleanup.

## Backend identity, permissions and side effects

The maintained harness at `scripts/ci/isolated-services.py:246` supplies a random
32-lowercase-hex run ID, password/token, `{}` config, database
`agent_bus_test_<run_id>`, localhost URLs with explicit ephemeral nonreserved
ports, and `agent-bus-test-<run_id>` service ID. It verifies readiness,
HTTP PID/service ID, actual PG/Redis scheme/host/port/path, and authenticated
`/admin/service` before returning readiness. Mesh fixture checks independently
repeat those identities before DB/Redis writes (`mesh_store_tests.rs:173`).
The refused port set is 6379/6380/5432/5300/8400/8401/18400, not just the three
older shared helper ports. Missing/unreachable/wrong identity fails, never skips.

In runner-container mode the maintained harness currently requests postgres:16
and redis:7-alpine. Those mutable tags are **not** latest-version or digest-pinned
qualification. Root must record resolved backend image IDs/digests from the
owned create receipts before execution and preserve them. Do not silently change
versions in this lane: digest pinning and a current-version compatibility matrix
are explicit follow-ups for maintained harness review. If images are absent and
network policy prevents pulls, HOLD; never fall back to a fleet service.

PG credentials are the disposable superuser: DDL/schema/history writes, trigger
failure injection, row locks and reconnect tests require it. Redis is disposable
and permits SCRIPT/EVAL, PUBSUB, ACL SETUSER/DELUSER, TIME and CLIENT ID/KILL.
The ACL-denied-TIME case restricts only its owned temporary user and asserts no
effects; reconnect injection kills only the captured owned client. Real tests
exercise durable history, collision atomicity, concurrent commit order, cursor
CAS, restart, NoOffsite, direct/group privacy, pending repair/quarantine, ACK
original-time deadlines and ACK-before-original, notification identity, capture
failures and legacy replay protection. No FLUSHALL, fleet client kill, claim
replication or global exclusive ownership assertion is part of this plan.

## Failure, deadlines and cleanup contract

Use finite overall and per-phase budgets, with owned signal/handle settlement.
The maintained harness bounds Docker prerequisites15s, readiness60s, Docker
commands180s by default, HTTP build/test phases1800s each. Caller interruption
must allow its SIGTERM/KeyboardInterrupt cleanup; do not treat docker CLI timeout
or HTTP listener closure as proof of child/container exit. A hard kill requires
Root's independently retained exact IDs and process handle to finish cleanup.

For evidence-preserving orchestration, prefer importing the unchanged maintained
module and calling `Services`/`execute` using a new persistent owned directory
under `/meshqa`, rather than reimplementing startup/selection/cleanup. Add
`/source/scripts/ci` to sys.path to resolve wine_lifecycle. Apply the same
`require_local_docker` and inspected runner-ID checks as `main`. Create the
Services once, start/execute inside try, and call its close exactly once in both
success and failure paths. Persist a sanitized record of captured fixture IDs,
labels, HTTP PID/exit/readiness and endpoint identities (no password/userinfo or
token) before cleanup. If using the CLI directly, Root must capture its exact
owned IDs and HTTP lifecycle out of band before temporary-directory removal.
The CLI's TemporaryDirectory otherwise deletes HTTP logs/cidfiles even on failure;
its console is not a complete retained lifecycle receipt.

The harness owns terminate/wait10s/kill/wait10s for its original HTTP child and
removes only captured 64-hex IDs plus validated cidfiles from its private scope
(`close`, line386). Preserve the first test/readiness failure even if cleanup
also fails; separately record all cleanup failures. Redact generated password
and token in stdout, stderr and retained HTTP logs. Never serialize subprocess
argv/env/CalledProcessError strings. Do not publish raw health JSON.

Root must additionally verify the HTTP child exited, each captured backend ID
is absent, the QA runner settled with PID0, output pipes reached EOF, the exact
runner ID was removed, and its own sccache server stopped if used. No remove by
name, wildcard, prune, port owner or other agent's IDs. If unsettled, preserve
the ID and evidence, mark HOLD and complete bounded exact-owner cleanup.
No shared cache or primary source is removed. Only after all checks pass can
Root mark this snapshot disposable-backend QA PASS; deployment remains NOT_RUN.

## Remaining gates / owners

- Root: final source manifest; actual Docker CLI/socket/image admission; cache
  release; execution and first failures; owned cleanup; integration-specific
  HTTP/router/supervisor tests; existing quality gates; artifact provenance.
- Maintained harness owner: mutable backend image pin/current-version matrix,
  retained sanitized fixture lifecycle receipts and HTTP logs, optional native
  `--locked` forwarding. These are visible gaps, not patched in private code.
- Fleet deployment owner: independent local PG/Redis rather than hidden remote
  forwarding, local messaging during peer and ASUS outage, reconnect convergence,
  Windows ACL/token admission, host restart and supervisor exit proof. A fixture
  pass must not be presented as live P1/ASUS/fleet readiness.
