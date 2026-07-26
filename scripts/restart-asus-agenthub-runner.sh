#!/usr/bin/env bash
set -euo pipefail

compose_file="${1:-/home/damartel/ci-runner-x64/repo-runners/agenthub.yml}"
container_name="asuspro13-agenthub-x64"
repository="David-Martel/agent-hub"
compose_project="runner-agenthub"
docker_socket="/var/run/docker.sock"

if [[ ! -f "$compose_file" ]]; then
  echo "Runner compose file not found: $compose_file" >&2
  exit 2
fi
if [[ ! -S "$docker_socket" ]]; then
  echo "Docker socket not found: $docker_socket" >&2
  exit 2
fi

existing_runner="$(
  gh api "repos/$repository/actions/runners?per_page=100" --paginate \
    --jq ".runners[] | select(.name == \"$container_name\") | [.id, .busy] | @tsv" |
    head -n 1
)"
if [[ -n "$existing_runner" ]]; then
  read -r existing_runner_id existing_runner_busy <<<"$existing_runner"
  if [[ "$existing_runner_busy" == "true" ]]; then
    echo "Refusing to replace busy runner: $container_name" >&2
    exit 3
  fi
  gh api --method DELETE \
    "repos/$repository/actions/runners/$existing_runner_id"
fi

runner_token="$(
  gh api --method POST "repos/$repository/actions/runners/registration-token" --jq .token
)"
docker_socket_gid="$(stat -c '%g' "$docker_socket")"
trap 'unset runner_token' EXIT
RUNNER_TOKEN="$runner_token" DOCKER_SOCKET_GID="$docker_socket_gid" \
  docker compose --project-name "$compose_project" -f "$compose_file" \
  run --rm --no-deps --user root --entrypoint chown "$compose_project" \
  -R runner:runner \
  /home/runner/actions-runner/_work \
  /home/runner/.cargo \
  /home/runner/.rustup \
  /home/runner/.local/bin \
  /home/runner/.cache/sccache/agent-hub \
  /home/runner/.cache/uv
RUNNER_TOKEN="$runner_token" DOCKER_SOCKET_GID="$docker_socket_gid" \
  docker compose --project-name "$compose_project" \
  -f "$compose_file" up -d --force-recreate
unset runner_token
trap - EXIT

for _ in {1..15}; do
  if docker inspect "$container_name" --format '{{.State.Running}}' 2>/dev/null |
      grep -qx true &&
    docker inspect "$container_name" --format '{{range .Mounts}}{{println .Destination}}{{end}}' |
      grep -qx /var/run/docker.sock &&
    docker exec "$container_name" grep -Eq \
      "^Groups:.*[[:space:]]${docker_socket_gid}([[:space:]]|$)" /proc/1/status &&
    docker exec --user runner "$container_name" docker info >/dev/null &&
    docker exec "$container_name" pgrep -af Runner.Listener >/dev/null; then
    echo "$container_name is active with Docker CLI/socket access"
    exit 0
  fi
  sleep 2
done

echo "$container_name did not become ready within 30 seconds" >&2
exit 1
