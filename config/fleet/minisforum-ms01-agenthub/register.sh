#!/usr/bin/env bash
# One-shot registration of ci-agenthub-runner@<instance> on minisforum-ms01. The token arrives on stdin
# only; it never appears in argv, the environment of a long-lived process, or a persistent file.
#   gh api -X POST repos/David-Martel/agent-hub/actions/runners/registration-token --jq .token |
#     ssh minisforum-ms01 'cd /tmp; exec sudo -n -u ci-agenthub env HOME=/srv/workspaces/ci-agenthub \
#       bash /srv/workspaces/ci-agenthub/.local/libexec/ci-agenthub/register.sh agenthub-docker-1'
# Run this only after the qualification in README.md has passed: registering with the `docker` label
# routes real jobs to the runner at once.
set -euo pipefail
inst=${1:-}
[[ "$inst" =~ ^[a-z0-9][a-z0-9-]{0,40}$ ]] || { echo "usage: register.sh <instance>" >&2; exit 64; }
[ "$(id -un)" = ci-agenthub ] || { echo "run as ci-agenthub" >&2; exit 77; }
XDG_RUNTIME_DIR=/run/user/$(id -u)
export XDG_RUNTIME_DIR
envf="$HOME/.config/ci-agenthub/$inst.env"
REPO=$(sed -n 's/^REPO=//p' "$envf")
labels=$(sed -n 's/^RUNNER_LABELS=//p' "$envf")
# This carrier serves agent-hub only, with exactly the labels the placement table names.
[ "$REPO" = David-Martel/agent-hub ] || { echo "REPO must be David-Martel/agent-hub" >&2; exit 65; }
[ "$labels" = self-hosted,Linux,X64,docker,minisforum-ms01 ] || { echo "unexpected labels: $labels" >&2; exit 65; }
tf=$(mktemp "$XDG_RUNTIME_DIR/agenthub-token.XXXXXX")
trap 'rm -f "$tf"' EXIT
chmod 0600 "$tf"
cat >"$tf"
[ -s "$tf" ] || { echo "empty token on stdin" >&2; exit 2; }
# Same user namespace as the unit (keep-id), so the registration volume is owned by the serving user.
podman run --rm --pull=never --health-interval disable \
  --userns=keep-id:uid=1000,gid=1000 \
  -e REPO="$REPO" -e RUNNER_NAME="minisforum-ms01-$inst" -e RUNNER_LABELS="$labels" \
  -e VIGIL_RUNNER_REQUIRE_DOCKER=0 \
  -v "minisforum-ms01-$inst-registration":/home/runner/.actions-runner-state \
  -v "$tf":/run/secrets/runner-registration-token:ro \
  localhost/vigil-utils-runner:latest register
