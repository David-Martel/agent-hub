#!/usr/bin/env bash
# Keep an SSH local forward open to an on-site agent-bus hub.
#
# Usage: agent-bus-tunnel.sh <jump> <target-host:port> [local-port] [timeout-seconds]
#   jump    ssh destination (ssh_config alias or user@host; use ssh_config for a port)
#   target  hub host:port as seen from the jump host
#
# Starts `ssh -f -N -L` only when nothing answers on 127.0.0.1:<local-port>/health,
# then waits until the forward answers. See agent-bus-tunnel.ps1 for the matching
# hub candidate and the loopback-token caveat.
set -euo pipefail

jump=${1:?usage: agent-bus-tunnel.sh <jump> <target-host:port> [local-port] [timeout]}
target=${2:?usage: agent-bus-tunnel.sh <jump> <target-host:port> [local-port] [timeout]}
port=${3:-18400}
timeout=${4:-20}
health="http://127.0.0.1:${port}/health"

case "$target" in
  *:[0-9]*) ;;
  *) echo "agent-bus tunnel: target must be host:port" >&2; exit 2 ;;
esac

hub_up() { curl -fs -m 3 -o /dev/null "$health" 2>/dev/null; }

if hub_up; then
  echo "agent-bus tunnel: $health already answers"
  exit 0
fi

# Detach ssh from the caller's stdio so a backgrounded forward never holds a
# pipe open; its diagnostics go to a log instead.
log="${TMPDIR:-/tmp}/agent-bus-tunnel-${port}.log"
if ! ssh -f -N \
  -o BatchMode=yes -o ExitOnForwardFailure=yes \
  -o ServerAliveInterval=30 -o ServerAliveCountMax=3 \
  -L "127.0.0.1:${port}:${target}" "$jump" </dev/null >/dev/null 2>"$log"; then
  echo "agent-bus tunnel: ssh failed; see $log" >&2
  exit 1
fi

for _ in $(seq 1 $((timeout * 2))); do
  if hub_up; then
    echo "agent-bus tunnel: $health answers"
    exit 0
  fi
  sleep 0.5
done
echo "agent-bus tunnel: no /health answer on $health within ${timeout}s" >&2
exit 1
