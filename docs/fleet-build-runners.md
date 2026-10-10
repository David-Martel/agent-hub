# Fleet build runners

Linux build jobs and the Windows cross-build use the fleet. Workflows selecting
fleet runners must use all default labels rather than the ambiguous
`self-hosted` label. The Windows build, unit and smoke jobs run on the Linux
`windows-cross` lane (mingw-w64 cross-compile to `x86_64-pc-windows-gnu`, tests
under Wine). Native Windows validation is a manual dispatch.

| Target | Runner labels | Intended host | Work |
| --- | --- | --- | --- |
| Linux x86-64 | `self-hosted`, `Linux`, `X64` | ASUSPRO13 | format, lint, unit, integration, and native builds |
| Linux x86-64 Docker | `self-hosted`, `Linux`, `X64`, `docker` | ASUSPRO13 | Linux-container build on the ASUS Docker engine |
| Linux ARM64 | `self-hosted`, `Linux`, `ARM64`, `fleet-build` | Spark fleet | native and Docker builds |
| Windows x86-64 build and smoke (cross) | `self-hosted`, `Linux`, `X64`, `docker`, `windows-cross` | qualified ASUSPRO13 runner; vigil1 only after the same qualification | GNU-target compile, strict clippy, unit tests, configured MCP and CLI/HTTP smoke under Wine, release artifacts |
| Windows x86-64 native (manual) | `self-hosted`, `Windows`, `X64`, `local-build` | dtm-p1gen7 | `workflow_dispatch` only: MSVC-ABI build, Codex config validator and native smoke |

Never put a Windows path such as `T:\RustCache` in workflow-level environment
variables. Windows-only paths belong in a Windows job. Linux jobs use a private
target directory under `runner.temp`.

## sccache policy

Native Linux runners must have `sccache` installed and reachable from `PATH`.
`scripts/ci/setup-rust.sh` verifies it before setting `RUSTC_WRAPPER`. An
unhealthy cache falls back to ordinary Cargo in Linux setup. Windows setup
instead requires its strict cache proof and fails without an uncached fallback.
The same bootstrap adds `$HOME/.local/bin` and `$HOME/.cargo/bin` to `PATH` and
bootstraps rustup without a default toolchain when a runner cache volume contains
no usable Cargo shim. It then installs the compiler pinned by
[`rust-toolchain.toml`](../rust-toolchain.toml) (currently Rust 1.98.1, plus
rustfmt and clippy), which is the one every job
actually uses; the Windows `setup-rust.ps1` does the same. Changing the pin
invalidates each runner's compiled cache once, since sccache keys include the
compiler.

The GNU cross lane uses `scripts/ci/setup-windows-cross.ps1` instead of the
permissive native Linux bootstrap. It requires the qualified immutable runner
image, its source revision and Dockerfile digest to match the running container.
It uses the pinned upstream Linux sccache 0.18.0 archive, the dedicated port 4228,
and `windows-cache-policy.json`. Two real explicit GNU-target compiler requests
must produce a Rust cache hit, identical artifact hashes and no new error
counters. A missing or unhealthy cache fails the job; setup never stops, resets
or replaces a serving daemon and never falls back to an uncached cross build.
Each immutable image has its own persistent child under the private
`~/.cache/agent-hub/windows-gnu0.18` directory. Image upgrades preserve older
children; an existing daemon serving another child causes setup to fail.
The source/image, compiler, linker and cache receipt is embedded in release
provenance, together with the actual PE DLL import lists. Native Windows DLL
acceptance remains a separate deployment gate.

Before adding `windows-cross` to a runner, qualify the rebuilt image and record
its immutable image ID, full source revision and Dockerfile SHA-256. Supply those
as `AGENT_HUB_WINDOWS_CROSS_IMAGE_ID`,
`AGENT_HUB_WINDOWS_CROSS_IMAGE_SOURCE_REVISION` and
`AGENT_HUB_WINDOWS_CROSS_IMAGE_DOCKERFILE_SHA256`. Build the image with matching
`org.opencontainers.image.revision` and `com.dtm.source.dockerfile-sha256` labels.
Qualification includes real positive and negative strict-Clippy/cache controls,
the GNU DLL import/runtime checks, configured shipping MCP launch and actual
fixture network identity. A tag name or the presence of Wine alone does not
qualify an image. Do not add the label before the receipt is accepted.

The safe L0 is a persistent, runner-local cache at
`~/.cache/sccache/agent-hub`. Spark jobs add the private-QSFP Redis endpoint at
`10.55.152.2:6381` as L1 through `SCCACHE_MULTILEVEL_CHAIN=disk,redis`. The
setup script probes the endpoint and falls back to disk-only caching if it is
unreachable. Do **not** share `CARGO_TARGET_DIR` between
machines or architectures. Rust target directories contain architecture- and
toolchain-specific build state and are not a network cache protocol.

Redis keys use `agent-hub:<runner-arch>:v1:` and expire after 14 days. Bump the
version component when compiler trust, cache format, or runner provenance
changes; retire the previous prefix after active builds finish. Do not flush
the entire Redis instance because other fleet repositories may have their own
prefixes.

The `windows-cross` image provides `gcc-mingw-w64-x86-64` (posix threads), Wine,
PowerShell 7, and the `x86_64-pc-windows-gnu` Rust target for the pinned
toolchain. The jobs set `CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine` and give
each step its own `timeout-minutes`, because one paused-time tokio test hung once
under Wine in a measurement run (see the vigil-utils evidence for the windows-cross
lane). Strict Windows Clippy disables the compiler wrapper, as before.

What the lane proves and does not prove. It gives cross-target compile, lint and
unit-test evidence plus an `.exe` smoke under Wine. It is not native Windows
acceptance: Wine's `sc.exe`, the Service Control Manager, registry semantics,
ACLs and the Windows TLS trust store are not real Windows. Provenance therefore
records `validation=wine` and `native_windows_validated=false`. The
`Windows Native Validation (manual)` job on dtm-p1gen7 runs the MSVC-ABI build, the
Codex config validator and the native smoke when a person dispatches the workflow.

Release artifacts change ABI. Tagged releases previously shipped MSVC-ABI
executables from a native Windows runner. They now ship `x86_64-pc-windows-gnu`
executables cross-built on the lane, listed in `BUILD-windows-x64.txt` beside the
checksums. The owner chose the GNU target; an MSVC cross build with cargo-xwin
remains available for a job that needs it.

The required `CLI And HTTP Smoke` job runs on the same lane and downloads the
artifacts from the successful `Windows Build And Unit Tests` job. Its owned
Redis/PostgreSQL fixtures require a local Linux Docker engine on the runner. The
smoke gate continues to fail on unavailable fixtures or a runtime revision
mismatch.

Wine HTTP smoke uses `isolated-services.py smoke --wine` with an exclusive fresh
prefix per invocation. Its Unix launcher PID is separate from the Windows guest
PID. Readiness requires the guest process inventory from that exact prefix,
the per-run service nonce, matching disposable backend endpoints, and an
authenticated admin readback. The prefix owner, mode, device and inode are fixed
before initialization and checked before and after guest queries and settlement.
An exited launcher cannot satisfy cleanup: the guest must disappear, its listener
must close, and the prefix-scoped `wineserver -w` must finish within the deadline.
Failures retain the owned prefix and harness directory. There is no global
`wineserver -k`, process-name kill, or deletion of another prefix.
All Wine initialization, queries, waits and guest launches use an explicit
private HOME under the owned harness directory. Host USERPROFILE, APPDATA,
LOCALAPPDATA, credential variables and loader/Wine path overrides are excluded.
Finite initialization/query/wait clients have a deadline and one shared two-second
settlement budget. Only their original direct child may be killed on timeout;
an exited client with a descendant retaining its pipes fails within the budget
and leaves that descendant untouched. The long-lived HTTP guest is supervised
separately by the outer harness and its guest/prefix settlement contract.

The unit-test and executable-version steps use `scripts/ci/wine_job.py` with
the same owned provider. Each group has a fresh prefix, private HOME and closed
store configuration; it consumes the compiler, cache and tool-home bindings
from setup's proof. Both Cargo test selections remain unchanged. The original
15-minute unit and 5-minute version limits include prefix initialization and
settlement. A successful launcher exit cannot pass a group until its exact
prefix has settled; failures retain custody evidence. Release provenance reads
the version receipt written by the settled group.

Cross setup exports the qualified full `AGENT_BUS_CI_RUNNER_CONTAINER_ID` for
the containerized smoke job. The harness
uses the selected local Docker daemon to inspect that exact running container,
then compares its kernel boot ID and network namespace identity to the harness.
Only after both match does it create fixtures with `--network container:<ID>`.
An arbitrary running ID or a Unix Docker proxy to another VM cannot qualify.
A separately qualified native Linux host may explicitly select
`--fixture-network native-host` without a container ID; its fixtures use the
existing ephemeral loopback port contract. Neither mode falls back to fleet
backend endpoints.

The lifecycle controls use synthetic providers and prove refusal/settlement
logic only. Actual Wine tasklist support, prefix lifetime, path round trips,
guest configuration readback, runtime DLLs and the downloaded Windows artifacts
still require an immutable runner image and a real source-bound smoke run. Until
that qualification is recorded, real Wine runtime validation is **NOT_TESTED**.


Every CI release artifact includes the immutable workflow commit, runner, target
directory, toolchain, exact cache executable/version, cache statistics, and
SHA-256 hashes. CI exports that commit as `AGENT_BUS_BUILD_REVISION`; the build
scripts rerun when it changes, and the downloaded-artifact HTTP smoke rejects
a `/health` revision mismatch. This prevents a warm Cargo cache from publishing
stale runtime identity. Tagged GitHub Release builds export the same immutable
revision and verify it in both the CLI and HTTP binaries before publication.

## Public-repository trust boundary

Fleet runners and their Redis cache are trusted infrastructure. The current
[`ci.yml`](../.github/workflows/ci.yml) runs on `pull_request`, pushes to `main`,
and explicit manual dispatch. PR events provide their own check contexts;
feature-branch pushes do not trigger a second matrix. Superseded PR runs may be
cancelled, while a running main validation is allowed to finish.

These triggers select validation events, not a complete authorization boundary.
The Windows cross-build job runs on a self-hosted Linux runner without developer,
live-bus, cloud-token or signing credentials. Other PR jobs, including the
Windows smoke, route to self-hosted runners; the workflow does not contain a
repository-ownership filter for fork PRs. Do not claim that fork execution is
prevented by the YAML. Runner access and any GitHub approval controls must be
verified before permitting untrusted code to execute on fleet infrastructure.
Review untrusted contributions without execution until the CI owner has
established an acceptable isolation and authorization boundary. Do not use
`pull_request_target` to execute untrusted code with trusted credentials.

Ordinary CI pins live storage URLs to closed ports and clears fleet routing and
authentication settings. Its backend integration job creates disposable
services and the tests refuse the live bus ports. This protects the ordinary
test configuration from accidentally reaching the live bus; it does not make
arbitrary PR code safe or prove that a runner's filesystem, Docker socket,
network, or shared cache is isolated. Never provide developer credentials or
live bus access to an ordinary validation job. The current workflow and
runner provisioning must be reviewed together before changing this boundary.

A network cache beyond this link-local Redis service may be enabled by
configuring a supported authenticated sccache backend in the runner service
environment. Backend credentials must remain on the runner or in an approved
secret store; they must not be committed or echoed by Actions. Roll out another
remote backend only after proving:

1. both Spark nodes use the same supported sccache version and backend;
2. cache traffic uses the dedicated QSFP addresses and not the control network;
3. credentials and backend storage are least-privilege;
4. concurrent ARM64 builds produce identical binaries with and without cache;
5. backend loss causes a clean local-compile fallback.

The two Spark disk-cache directories remain host-local; Redis is the network
cache protocol. Comments in a host config are not proof of an NFS mount or
distributed cache. Use `findmnt -T ~/.cache/sccache`, test the Redis endpoint,
and inspect `sccache --show-stats` as acceptance checks.

## Runner registration

Repository runners should use default OS and architecture labels plus an
operator label such as `fleet-build`. Run one listener per registered runner;
do not reuse a runner directory already assigned to another repository.

After registration, confirm the repository reports the runner online and that
the labels match the host:

```bash
uname -m
command -v cargo sccache
sccache --show-stats
```

ARM64 jobs intentionally target the default `ARM64` label. A Spark registered
without that label will not receive work and should be repaired at the runner,
not worked around with an ambiguous workflow target.

Only runners assigned Docker jobs need Docker access. The repository's
containerized ASUS runner mounts the host Docker CLI and socket and carries the
explicit `docker` label for x86-64 validation. Spark runners provide native
ARM64 Docker coverage. The Windows runner does not service Linux Docker jobs.
Use `scripts/restart-asus-agenthub-runner.sh` for replacement; it refuses to
interrupt a busy runner, removes an idle stale registration, initializes volume
ownership, and verifies that the listener process retains the socket group.

Trusted branch CI compiles every Criterion target but does not run full
performance sampling on each push. Run benchmarks intentionally on an idle
ASUS runner (or a dedicated manual workflow) so sampling does not serialize
unrelated format, lint, integration, and smoke jobs.
