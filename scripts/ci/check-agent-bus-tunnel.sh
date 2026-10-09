#!/usr/bin/env bash
# Behaviour test for scripts/agent-bus-tunnel.sh, using fake `ssh` and `curl`.
#
# Protects: the tunnel helper's ownership of the ssh process it starts.
# Detects: a timeout that kills processes it did not start (the old
# `pkill -f -- "-L <forward>"` killed any process whose command line held the
# forward spec, such as another agent's tunnel); an ssh that exits early being
# reported as a timeout; a forward left running after a timeout; a second ssh
# started when the hub already answers.
# Needs: bash, a POSIX userland; no network, no real ssh.
# Breadcrumb: docs/network-location-routing.md "Keeping the forward up".
#
# Usage: check-agent-bus-tunnel.sh [script-under-test]
set -euo pipefail

repo="$(git rev-parse --show-toplevel)"
helper="$(cd "$(dirname "${1:-$repo/scripts/agent-bus-tunnel.sh}")" && pwd -P)/$(basename "${1:-agent-bus-tunnel.sh}")"
work="$(mktemp -d "${RUNNER_TEMP:-/tmp}/tunnel-check.XXXXXX")"
pids=()
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  [[ -f "$work/ssh.pid" ]] && kill "$(cat "$work/ssh.pid")" 2>/dev/null || true
  rm -rf -- "$work"
}
trap cleanup EXIT

mkdir "$work/bin"
# Fake ssh: records its PID, marks the forward up, then waits. FAKE_SSH_MODE=fail exits at once.
cat > "$work/bin/ssh" <<'SSH'
#!/usr/bin/env bash
# Like real ssh, -f means: go to the background and return 0 (the old helper used it).
for a in "$@"; do
  if [[ "$a" == -f ]]; then
    args=(); for b in "$@"; do [[ "$b" == -f ]] || args+=("$b"); done
    "$0" "${args[@]}" </dev/null >/dev/null 2>&1 &
    exit 0
  fi
done
echo $$ > "$FAKE_STATE/ssh.pid"
[[ "${FAKE_SSH_MODE:-}" == fail ]] && { echo "fake ssh: forward refused" >&2; exit 255; }
[[ "${FAKE_SSH_MODE:-}" == up ]] && touch "$FAKE_STATE/up"
# Wait without exec, so this process keeps ssh's argv (with -L) the way real ssh does.
sleep 300 &
nap=$!
trap 'kill "$nap" 2>/dev/null; exit 143' TERM
wait "$nap"
SSH
# Fake curl: /health answers only when the state file exists.
cat > "$work/bin/curl" <<'CURL'
#!/usr/bin/env bash
[[ -f "$FAKE_STATE/up" ]]
CURL
chmod +x "$work/bin/ssh" "$work/bin/curl"

port=18999
forward="127.0.0.1:${port}:127.0.0.1:9"
run_helper() {
  TMPDIR="$work" PATH="$work/bin:$PATH" FAKE_STATE="$work" bash "$helper" jump-alias 127.0.0.1:9 "$port" 1
}
fail() { echo "FAIL: $*" >&2; exit 1; }
alive() { kill -0 "$1" 2>/dev/null; }
reset() { rm -f "$work/ssh.pid" "$work/up"; }

# A decoy whose command line contains the same forward spec, like another agent's tunnel.
bash -c 'sleep 300 & n=$!; trap "kill \$n 2>/dev/null; exit 143" TERM; wait $n' decoy-tunnel -N -L "$forward" other-jump &
decoy=$!
pids+=("$decoy")
sleep 0.2
alive "$decoy" || fail "decoy did not start"

echo "case 1: timeout stops only the helper's own ssh"
reset
set +e; FAKE_SSH_MODE=hang run_helper 2>"$work/err"; rc=$?; set -e
[[ $rc -eq 1 ]] || fail "timeout returned $rc, want 1"
own="$(cat "$work/ssh.pid")"
sleep 0.2
alive "$own" && fail "helper left its own ssh ($own) running after a timeout"
alive "$decoy" || fail "helper killed a process it did not start (decoy $decoy)"
grep -q "stopped ssh pid $own" "$work/err" || fail "timeout message does not name the stopped pid"

echo "case 2: ssh that exits early is reported as an ssh failure"
reset
set +e; FAKE_SSH_MODE=fail run_helper 2>"$work/err"; rc=$?; set -e
[[ $rc -eq 1 ]] || fail "early ssh exit returned $rc, want 1"
grep -q "ssh exited before" "$work/err" || fail "early ssh exit was not reported as such"

echo "case 3: a forward that answers is left running"
reset
FAKE_SSH_MODE=up run_helper >"$work/out" || fail "answering forward returned non-zero"
own="$(cat "$work/ssh.pid")"
alive "$own" || fail "helper did not leave the answering forward running"
kill "$own"; wait "$own" 2>/dev/null || true

echo "case 4: an answering hub starts no ssh"
reset
touch "$work/up"
run_helper >"$work/out" || fail "already-up returned non-zero"
[[ -f "$work/ssh.pid" ]] && fail "helper started ssh although /health answered"
grep -q "already answers" "$work/out" || fail "already-up message missing"

alive "$decoy" || fail "decoy died during the run"
echo "agent-bus tunnel checks: 4 passed"
