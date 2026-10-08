#!/usr/bin/env bash
# Keep an SSH local forward open to an on-site agent-bus hub (Linux/macOS).
#
# Usage: agent-bus-tunnel.sh <jump> <target-host:port> [local-port (default 18480)] [timeout-seconds]
#   jump    ssh destination (ssh_config alias, user@host or ssh://user@host:port)
#   target  hub host:port as seen from the jump host
#
# Starts `ssh -f -N -L` only when nothing answers on 127.0.0.1:<local-port>/health,
# then waits until the forward answers; on timeout the new forward is killed.
# See agent-bus-tunnel.ps1 (Windows; Windows OpenSSH cannot detach with -f) for
# the matching hub candidate and the loopback-token caveat.
set -euo pipefail

usage="usage: agent-bus-tunnel.sh <jump> <target-host:port> [local-port] [timeout-seconds]"
jump=${1:?$usage}
target=${2:?$usage}
port=${3:-18480}
timeout=${4:-20}

[[ "$jump" != -* ]] || { echo "agent-bus tunnel: jump must not start with '-'" >&2; exit 2; }
target_re='^[][A-Za-z0-9.:-]+:[0-9]+$'
[[ "$target" =~ $target_re ]] || { echo "agent-bus tunnel: target must be host:port" >&2; exit 2; }
[[ "$port" =~ ^[0-9]{1,5}$ ]] && (( 10#$port >= 1 && 10#$port <= 65535 )) || { echo "agent-bus tunnel: invalid local port" >&2; exit 2; }
[[ "$timeout" =~ ^[0-9]{1,4}$ ]] && (( 10#$timeout >= 1 )) || { echo "agent-bus tunnel: invalid timeout" >&2; exit 2; }
port=$((10#$port))
timeout=$((10#$timeout))

health="http://127.0.0.1:${port}/health"
forward="127.0.0.1:${port}:${target}"
hub_up() { curl -fs -m 3 -o /dev/null "$health" 2>/dev/null; }

if hub_up; then
  echo "agent-bus tunnel: $health already answers"
  exit 0
fi

# Detach ssh from the caller's stdio so a backgrounded forward never holds a
# pipe open; its diagnostics go to a private log instead.
log=$(mktemp "${TMPDIR:-/tmp}/agent-bus-tunnel.XXXXXX")
if ! ssh -f -N \
  -o BatchMode=yes -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=30 -o ServerAliveCountMax=3 \
  -L "$forward" -- "$jump" </dev/null >/dev/null 2>"$log"; then
  echo "agent-bus tunnel: ssh failed; see $log" >&2
  exit 1
fi

for _ in $(seq 1 $((timeout * 2))); do
  if hub_up; then
    echo "agent-bus tunnel: $health answers"
    rm -f "$log"
    exit 0
  fi
  sleep 0.5
done
# pkill matches a regex; escape the forward spec so IPv6 brackets match literally.
pkill -f -- "-L $(printf '%s' "$forward" | sed 's/[][\\.*^$]/\\&/g')" 2>/dev/null || true
echo "agent-bus tunnel: no /health answer on $health within ${timeout}s; forward stopped (log: $log)" >&2
exit 1
