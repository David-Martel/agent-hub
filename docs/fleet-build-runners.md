# Fleet build runners

The build fleet has three distinct target classes. Workflows must select all
default runner labels rather than using the ambiguous `self-hosted` label.

| Target | Runner labels | Intended host | Work |
| --- | --- | --- | --- |
| Linux x86-64 | `self-hosted`, `Linux`, `X64` | ASUSPRO13 | format, lint, unit, integration, and native builds |
| Linux x86-64 Docker | `self-hosted`, `Linux`, `X64`, `docker` | ASUSPRO13 | Linux-container build on the ASUS Docker engine |
| Linux ARM64 | `self-hosted`, `Linux`, `ARM64`, `fleet-build` | Spark fleet | native and Docker builds |
| Windows x86-64 | `self-hosted`, `Windows`, `X64`, `local-build` | dtm-p1gen7 | Windows compile and release artifacts |

Never put a Windows path such as `T:\RustCache` in workflow-level environment
variables. Windows-only paths belong in a Windows job. Linux jobs use a private
target directory under `runner.temp`.

## sccache policy

Each runner must have `sccache` installed and reachable from `PATH`.
`scripts/ci/setup-rust.sh` verifies it before setting `RUSTC_WRAPPER`. An
unhealthy cache falls back to ordinary Cargo instead of blocking CI.
The same bootstrap adds `$HOME/.local/bin` and `$HOME/.cargo/bin` to `PATH` and
installs a minimal stable rustup toolchain when a runner cache volume contains
no usable Cargo shim.

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

The dtm-p1gen7 Windows runner uses `scripts/ci/setup-rust.ps1` with persistent
Cargo and sccache directories under `%LOCALAPPDATA%\agent-hub-ci`. Its single
dedicated listener serializes Windows jobs, so those target outputs are never
written concurrently. Windows cache data remains local and is not mixed with
Linux or ARM64 objects.

Every release artifact includes the immutable workflow commit, runner, target
directory, toolchain, exact cache executable/version, cache statistics, and
SHA-256 hashes. CI exports that commit as `AGENT_BUS_BUILD_REVISION`; the build
scripts rerun when it changes, and the downloaded-artifact HTTP smoke rejects
a `/health` revision mismatch. This prevents a warm Cargo cache from publishing
stale runtime identity.

## Public-repository trust boundary

Fleet runners and their Redis cache are trusted infrastructure. The fleet
workflow (`ci.yml`) is triggered only by pushes to branches in this repository
and explicit manual dispatches. It must never regain a `pull_request` or
`pull_request_target` trigger.

Same-repository pull requests are validated by their trusted branch push
through `ci.yml`; a duplicate `pull_request` workflow would spend the same
fleet capacity twice. Fork code must never execute on ASUS, Spark,
dtm-p1gen7, Docker, or the fleet cache. Review a fork without execution, then
cherry-pick accepted commits onto a repository-owned branch to run the trusted
push matrix. Do not use `pull_request_target` to work around that boundary.

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
