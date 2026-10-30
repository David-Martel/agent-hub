#!/usr/bin/env bash
# Qualification of the ms01 ci-agenthub carrier BEFORE registration. Run as ci-agenthub:
#   sudo -u ci-agenthub env HOME=/srv/workspaces/ci-agenthub bash qualify.sh <agent-hub-commit>
# It starts the runner image UNREGISTERED with the unit's isolation flags (keep-id, the account's own
# Podman socket, cores, memory, idle priority), as the `runner` user a job runs as, and runs the bodies of agent-hub's three `docker`-label
# jobs from .github/workflows/ci.yml at that commit: docker-x64, test-integration and test-history.
# It never talks to GitHub Actions, so no real job can land on a half-qualified runner.
set -euo pipefail
commit=${1:?usage: qualify.sh <agent-hub-commit>}
[[ "$commit" =~ ^[0-9a-f]{7,40}$ ]] || { echo "commit must be a hex SHA" >&2; exit 64; }
[ "$(id -un)" = ci-agenthub ] || { echo "run as ci-agenthub" >&2; exit 77; }
XDG_RUNTIME_DIR=/run/user/$(id -u)
export XDG_RUNTIME_DIR
CORES=${CORES:-4-7}
MEMORY=${MEMORY:-12G}
qual="$HOME/ci-scratch/qualification"
mkdir -p "$qual"
if [ ! -d "$qual/agent-hub/.git" ]; then
  git clone --quiet --filter=blob:none https://github.com/David-Martel/agent-hub "$qual/agent-hub"
fi
git -C "$qual/agent-hub" fetch --quiet origin
git -C "$qual/agent-hub" checkout --quiet --detach "$commit"
git -C "$qual/agent-hub" clean -fdxq

podman rm -f --ignore minisforum-ms01-agenthub-qualification >/dev/null
# shellcheck disable=SC2016 # the single-quoted job body expands inside the container, not here
exec nice -n 19 chrt --idle 0 podman run --rm --pull=never --name minisforum-ms01-agenthub-qualification \
  --health-interval disable \
  --userns=keep-id:uid=1000,gid=1000 --user 1000:1000 \
  -e DOCKER_HOST=unix:///var/run/docker.sock -e CARGO_INCREMENTAL=0 -e RUNNER_ARCH=X64 \
  -e RUNNER_NAME=minisforum-ms01-agenthub-qualification \
  -v "$XDG_RUNTIME_DIR/podman/podman.sock":/var/run/docker.sock \
  -v "$qual":/home/runner/actions-runner/_work \
  --tmpfs /tmp:rw,nosuid,nodev,size=8g,mode=1777 \
  --memory "$MEMORY" --memory-swap "$MEMORY" --pids-limit 8192 \
  --workdir /home/runner/actions-runner/_work/agent-hub \
  --entrypoint taskset localhost/vigil-utils-runner:latest -c "$CORES" bash -c '
set -euo pipefail
export RUNNER_TEMP=/home/runner/actions-runner/_work/_temp
rm -rf "$RUNNER_TEMP"; mkdir -p "$RUNNER_TEMP"
export GITHUB_ENV=$RUNNER_TEMP/github-env GITHUB_PATH=$RUNNER_TEMP/github-path GITHUB_STEP_SUMMARY=$RUNNER_TEMP/summary
: >"$GITHUB_ENV"; : >"$GITHUB_PATH"; : >"$GITHUB_STEP_SUMMARY"
step() { printf "\n=== %s %s\n" "$(date -u +%FT%TZ)" "$*"; }
step "identity: $(id -un) uid=$(id -u) hostname=$(cat /etc/hostname) head=$(git rev-parse HEAD)"
docker version --format "docker client={{.Client.Version}} server={{.Server.Version}}"

step "docker-x64: docker build -f docker/Dockerfile.linux"
s=$SECONDS
docker build -f docker/Dockerfile.linux -t agent-bus-linux-validate .
echo "docker-x64 PASS in $((SECONDS - s)) s"

step "setup-rust.sh"
s=$SECONDS
bash scripts/ci/setup-rust.sh
# Apply what an Actions step would carry into the next step.
while IFS= read -r p; do [ -n "$p" ] && PATH="$p:$PATH"; done <"$GITHUB_PATH"
set -a; . "$GITHUB_ENV"; set +a
echo "setup-rust PASS in $((SECONDS - s)) s"

step "test-integration: isolated-services integration --network-container"
s=$SECONDS
python3 -B scripts/ci/isolated-services.py integration \
  --network-container "$(cat /etc/hostname)" --minimum-backend-tests 85
echo "test-integration PASS in $((SECONDS - s)) s"

step "test-history: isolated-services history --network-container"
s=$SECONDS
python3 -B scripts/ci/isolated-services.py history --network-container "$(cat /etc/hostname)"
echo "test-history PASS in $((SECONDS - s)) s"

step "leftover fixture containers (must be none)"
left=$(docker ps -a --filter label=agent-bus.test-run --format "{{.ID}}" | wc -l)
echo "fixture containers left: $left"
[ "$left" -eq 0 ]
step "QUALIFICATION PASS"
'
