# agent-hub Docker-capable runner on minisforum-ms01

This directory defines `minisforum-ms01-agenthub-docker-1`, a second carrier for agent-hub's
`[self-hosted, Linux, X64, docker]` jobs (`docker-x64`, `test-integration` and `test-history` in
[`ci.yml`](../../../.github/workflows/ci.yml)). Until now only ASUSPRO13 carried them. The owner
asked for fewer jobs on ASUSPRO13 and more on ms01, and on 2026-10-29 approved this carrier as an
isolated design (relayed on agent-bus `machine-hygiene`). The placement table in
[`docs/fleet-build-runners.md`](../../../docs/fleet-build-runners.md) lists it.

- **Install:** run `sudo bash setup-host.sh` on ms01 once. Then copy `ci-agenthub-runner@.service`
  to `~ci-agenthub/.config/systemd/user/`, the instance file `agenthub-docker-1.env` to
  `~ci-agenthub/.config/ci-agenthub/`, and `register.sh` and `qualify.sh` to
  `~ci-agenthub/.local/libexec/ci-agenthub/`.
- **Build:** there is no build. The runner uses ms01's existing `localhost/vigil-utils-runner:latest`
  unchanged. Copy it into the account with `podman save` from `vigil-ci` piped into `podman load` as
  `ci-agenthub`; see [Image provenance](#image-provenance).
- **Test:** `python3 -B -m unittest discover -s scripts/ci -p 'test_ms01_agenthub_runner.py'` runs
  [`test_ms01_agenthub_runner.py`](../../../scripts/ci/test_ms01_agenthub_runner.py), which pins the
  invariants below. CI's unit job discovers it with the other `scripts/ci` tests. `qualify.sh <commit>`
  is the host qualification.
- **Run:** run `qualify.sh` first, then `register.sh`, then
  `systemctl --user -M ci-agenthub@ enable --now ci-agenthub-runner@agenthub-docker-1.service`.

## Image provenance

The runner image is a `podman save` and `podman load` copy of ms01's `vigil-utils-runner` image,
made on 2026-10-29. It does not follow later vigil-utils rebuilds, so it will drift silently from
the `vigil-ci` copy.

| Field | Value |
|---|---|
| Tag | `localhost/vigil-utils-runner:latest` (in `ci-agenthub`'s store) |
| Image ID | `sha256:8a03877ccdf4d2554c99837e290b3254d185f954f82d0dcc9b258aa4fa78c5a7` |
| Digest | `sha256:9af2c970cbecc1d1c7e03828edeae25d7e99e2c1b1356fb1a8b140031c4b984c` |
| Source | vigil-utils `docker/` at `2287aa75282f5a9a9332b51304e5ee6fbb16d0ca` (label `org.opencontainers.image.revision`), built 2026-10-29 19:37 UTC |
| Tools used here | Docker CLI 29.5.3 and Buildx with the `docker-container` driver, git, Python 3, rustup and Cargo |

[agent-hub #213](https://github.com/David-Martel/agent-hub/issues/213) tracks an agent-hub-owned runner image as the long-term source. Until then,
check the tag against this ID before you trust a result:
`podman image inspect --format '{{.Id}}' localhost/vigil-utils-runner:latest`.

## Isolation

The job gets a Docker-compatible socket, and anyone who controls that socket controls every
container behind it. So the socket is the dedicated `ci-agenthub` account's own rootless Podman API
socket, `/run/user/2002/podman/podman.sock`, mounted at `/var/run/docker.sock`. It reaches only
that account's containers. It is never the shared `vigil-ci` account's socket, which would hand an
agent-hub job the bebird, flir, obsbot, vigil-spark and vigil-utils runner containers on this host.
It is not ms01's rootful Docker engine either.

`--userns=keep-id:uid=1000,gid=1000` maps the account onto the container's `runner` user (uid 1000),
which the image entrypoint drops to before it serves jobs, so the job can open the socket. The first start with keep-id spends a few minutes re-mapping the 7.7 GB
image's ownership; later starts reuse it.

## Limits

Jobs start sibling containers through the socket: postgres and redis fixtures, BuildKit, image
builds. They run under the account's Podman API service, not under the runner unit. So
`setup-host.sh` puts the bound where they land:

| Bound | Where | Value |
|---|---|---|
| Cores | `user-2002.slice` `AllowedCPUs`, `podman.service` `CPUAffinity`, the unit's `taskset` | 4-7 |
| CPU | `user-2002.slice` `CPUQuota` | 400 % |
| Memory | `user-2002.slice` `MemoryMax`; runner container `--memory` | 16 G; 12 G |
| Tasks | `user-2002.slice` `TasksMax`; runner `--pids-limit` | 4096; 8192 |
| Priority | the unit and `podman.service`: `Nice=19`, `CPUSchedulingPolicy=idle`, `IOSchedulingClass=idle` | idle |

Cores 4-7 are shared with `ms01-flir-streaming-camera-1`, which ran 3 jobs in the 48 h before
2026-10-29. They never overlap 8-11, the vigil-spark runner `minisforum-ms01-vigilspark-extra-1`.
The probe on 2026-10-29 confirmed the inheritance: the BuildKit container ran `SCHED_IDLE` with
affinity 4-7 inside `user-2002.slice`.

## Labels

The labels are exactly `self-hosted, Linux, X64, docker, minisforum-ms01`, and `register.sh` refuses
any other set. There is **no `windows-cross`**: the placement guide requires that image's
qualification receipt before the label goes on a runner.

## Qualification

Registering with `docker` routes real jobs to the runner at once. `qualify.sh` therefore runs the
three job bodies first, in the runner image, unregistered, with the unit's isolation flags.

Receipt (run 2026-10-29 23:48:57Z to 23:55:47Z on minisforum-ms01, as the runner user, uid 1000 in the
container, at agent-hub `f19b04ae5967d109a52b59551bf1b56443340871`, image ID `sha256:8a03877ccdf4…c5a7`):

| Job body | Result | Time |
|---|---|---|
| `docker-x64` (`docker build -f docker/Dockerfile.linux`) | PASS | 222 s |
| `scripts/ci/setup-rust.sh` | PASS | 17 s |
| `isolated-services` integration (`--network-container --minimum-backend-tests 85`) | PASS | 150 s |
| `isolated-services` history | PASS | 21 s |
| Fixture containers left afterwards | 0 | |

The full log is on asuspro13 at `~/dev/evidence/agenthub-ms01-docker-runner/qualification.log` (sha256 `24d6af9c813f9acc5ac162ba70859966acaddc5c0ca90242b4ce5a33c86e8f6c`).
An earlier attempt ran as root because the image entrypoint starts as root. I stopped it, and
`qualify.sh` now passes `--user 1000:1000`.

## Known differences from the ASUS carrier

- **The Docker check:** `VIGIL_RUNNER_REQUIRE_DOCKER=0`. The image entrypoint's Docker check expects
  a rootful engine's socket group and a Buildx plugin. The unit's `ExecStartPre` socket test and the
  qualification above take its place.
- **BuildKit:** `docker build` goes through the image's Buildx with its `docker-container` driver.
  Buildx starts a `buildx_buildkit_default` container in the account, and it stays up between
  jobs.
- **Shared build cache:** that container's build cache is shared by later jobs, including PR jobs.
  The ASUS engine's cache is shared the same way, and across repositories too. No `docker`-label
  job pushes an image or produces a release artifact. Release builds run on vigil1 and the Sparks.
- **Clean-up:** `docker buildx prune` in the account clears the cache.

## Rollout and retirement

The ASUS `docker` carriers stay. Retire them only after this runner has run real jobs green for a
day, and only through agent-hub's own runner configuration.
