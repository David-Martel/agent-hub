#!/usr/bin/env bash
# Deploy a built agent-bus to the Linux hub and restart agent-bus-http.service.
#
# The hub is the fleet's coordination point, so this is a two-phase tool:
#   (default) --dry-run  builds the ref in a throwaway worktree with its own
#                        target dir, compares versions and hashes with what is
#                        installed, backs the installed binaries up, then PRINTS
#                        the install, restart, readback and rollback steps. It
#                        never touches ~/.local/bin or the service.
#   --apply              runs those steps. Run it only in a restart window the
#                        hub owner scheduled.
#   --rollback <dir>     restores the binaries saved in a backup directory and
#                        restarts the service.
#
# Why a wrapper: install.sh deploys atomically but keeps no backup, never
# restarts, and its health check does not prove a message round trip. Both
# matter after agent-hub#194 (a send can succeed into a store nobody reads).
#
# Usage: scripts/hub-restart.sh [--ref REF] [--expect-sha SHA] [--apply]
#                               [--bin-dir DIR] [--state-dir DIR] [--unit NAME]
#        scripts/hub-restart.sh --rollback BACKUP_DIR [--apply]
# Exit codes: 0 ok, 1 failure or failed readback, 2 usage.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REF="origin/main"
EXPECT_SHA=""
APPLY=0
ROLLBACK_DIR=""
BIN_DIR="${HOME}/.local/bin"
STATE_DIR="${HOME}/.local/state/agent-bus-hub-restart"
UNIT="agent-bus-http.service"
PROBE_AGENT="${HUB_RESTART_AGENT:-hub-restart}"
BINARIES=(agent-bus agent-bus-http agent-bus-mcp)

log() { printf '%s\n' "$*"; }
err() { printf 'hub-restart: %s\n' "$*" >&2; }
usage() { sed -n '2,24p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --ref) REF="$2"; shift 2 ;;
        --expect-sha) EXPECT_SHA="$2"; shift 2 ;;
        --apply) APPLY=1; shift ;;
        --dry-run) APPLY=0; shift ;;
        --bin-dir) BIN_DIR="$2"; shift 2 ;;
        --state-dir) STATE_DIR="$2"; shift 2 ;;
        --unit) UNIT="$2"; shift 2 ;;
        --rollback) ROLLBACK_DIR="$2"; shift 2 ;;
        -h | --help) usage; exit 0 ;;
        *) err "unknown argument: $1"; usage >&2; exit 2 ;;
    esac
done

sha_of() { sha256sum "$1" | cut -d' ' -f1; }
# agent-bus-mcp has no --version: any argument starts the stdio server.
version_of() {
    case "$(basename "$1")" in
        agent-bus-mcp) printf 'agent-bus-mcp (no --version; sha256 %s)\n' "$(sha_of "$1" | cut -c1-12)" ;;
        *) "$1" --version 2>&1 | head -1 ;;
    esac
}

# Rows the stores disagree on right now (stream_length - pg_message_count), or "?".
store_gap() {
    "$BIN_DIR/agent-bus" health --encoding json 2>/dev/null | python3 -c '
import json, sys
try:
    h = json.load(sys.stdin)
    print(h["stream_length"] - h["pg_message_count"])
except Exception:
    print("?")
'
}
BASELINE_GAP="?"

# run STEP...: execute under --apply, print under --dry-run.
step() {
    if [[ $APPLY -eq 1 ]]; then
        log "+ $*"
        "$@"
    else
        log "[dry-run] would run: $*"
    fi
}

restart_and_verify() {
    step systemctl --user restart "$UNIT"
    verify_hub
}

# Readback after a restart. Each check fails the run (never just warns).
verify_hub() {
    log ""
    log "== readback =="
    if [[ $APPLY -eq 0 ]]; then
        log "[dry-run] would run, in order:"
        log "  systemctl --user show $UNIT -p ActiveState -p SubState -p ExecMainStartTimestamp"
        log "      expect ActiveState=active SubState=running and a start time after the restart"
        log "  agent-bus health --encoding json"
        log "      expect ok=true, pg_dropped_writes=0, pg_write_errors=0, and the"
        log "      stream_length - pg_message_count gap no larger than the baseline taken before"
        log "      the restart (now: $(store_gap)); a gap that already exists is the #194 backlog,"
        log "      only growth fails. This is counts only; the round trip below proves delivery"
        log "  agent-bus presence-list --encoding human"
        log "      expect the usual fleet agents back online (they re-announce within their TTL)"
        log "  agent-bus send --from-agent $PROBE_AGENT --to-agent $PROBE_AGENT --topic restart-probe --body <marker>"
        log "  agent-bus read --agent $PROBE_AGENT --topic restart-probe --since-minutes 5 --encoding json"
        log "      expect the marker back (a send alone can succeed into a dead backend)"
        return 0
    fi
    local state
    state="$(systemctl --user show "$UNIT" -p ActiveState -p SubState -p ExecMainStartTimestamp)"
    log "$state"
    grep -q '^ActiveState=active$' <<<"$state" || { err "unit is not active"; return 1; }
    grep -q '^SubState=running$' <<<"$state" || { err "unit is not running"; return 1; }
    local waited=0 health=""
    until health="$("$BIN_DIR/agent-bus" health --encoding json 2>/dev/null)" && [[ -n "$health" ]]; do
        sleep 1
        waited=$((waited + 1))
        [[ $waited -lt 30 ]] || { err "health did not answer within 30 s"; return 1; }
    done
    log "$health"
    VERIFY_HEALTH="$health" BASELINE_GAP="$BASELINE_GAP" python3 - <<'PY' || return 1
import json, os, sys
h = json.loads(os.environ["VERIFY_HEALTH"])
bad = []
if not h.get("ok"):
    bad.append("ok is not true")
for key in ("pg_dropped_writes", "pg_write_errors"):
    if h.get(key, 0) != 0:
        bad.append(f"{key}={h.get(key)}")
gap = h.get("stream_length", 0) - h.get("pg_message_count", 0)
base = os.environ["BASELINE_GAP"]
if base != "?" and gap > int(base):
    bad.append(f"store gap grew from {base} to {gap}")
if bad:
    print("health check failed: " + "; ".join(bad), file=sys.stderr)
    sys.exit(1)
PY
    "$BIN_DIR/agent-bus" presence-list --encoding human | head -20
    local marker
    marker="hub-restart-probe-$(date -u +%Y%m%dT%H%M%SZ)-$$"
    "$BIN_DIR/agent-bus" send --from-agent "$PROBE_AGENT" --to-agent "$PROBE_AGENT" \
        --topic restart-probe --body "$marker" >/dev/null
    if ! "$BIN_DIR/agent-bus" read --agent "$PROBE_AGENT" --topic restart-probe \
        --since-minutes 5 --limit 50 --encoding json | grep -q -- "$marker"; then
        err "round trip failed: probe $marker was sent but not read back"
        return 1
    fi
    log "round trip ok ($marker)"
}

# ── rollback ────────────────────────────────────────────────────────────────
if [[ -n "$ROLLBACK_DIR" ]]; then
    BASELINE_GAP="$(store_gap)"
    log "== rollback from $ROLLBACK_DIR (baseline gap $BASELINE_GAP) =="
    for bin in "${BINARIES[@]}"; do
        [[ -f "$ROLLBACK_DIR/$bin" ]] || { err "backup is missing $bin"; exit 1; }
        log "  $bin: $(version_of "$ROLLBACK_DIR/$bin") (sha256 $(sha_of "$ROLLBACK_DIR/$bin" | cut -c1-12))"
    done
    for bin in "${BINARIES[@]}"; do
        step cp --preserve=mode "$ROLLBACK_DIR/$bin" "$BIN_DIR/$bin.rollback"
        step mv -f "$BIN_DIR/$bin.rollback" "$BIN_DIR/$bin"
    done
    restart_and_verify
    exit 0
fi

# ── plan: resolve and build ────────────────────────────────────────────────
cd "$REPO_ROOT"
git rev-parse --verify --quiet "$REF^{commit}" >/dev/null || { err "unknown ref: $REF"; exit 2; }
SHA="$(git rev-parse "$REF^{commit}")"
if [[ -n "$EXPECT_SHA" && "$SHA" != "$EXPECT_SHA"* ]]; then
    err "ref $REF is at ${SHA:0:12}, expected $EXPECT_SHA"
    exit 1
fi
log "== plan =="
log "mode:      $([[ $APPLY -eq 1 ]] && echo APPLY || echo DRY-RUN)"
log "ref:       $REF -> ${SHA:0:12}  $(git log -1 --pretty=%s "$SHA")"
log "unit:      $UNIT"
log "bin dir:   $BIN_DIR"

WORK="$(mktemp -d "${TMPDIR:-/tmp}/hub-restart.XXXXXX")"
cleanup() {
    git -C "$REPO_ROOT" worktree remove --force "$WORK/src" >/dev/null 2>&1 || true
    rm -rf -- "$WORK"
}
trap cleanup EXIT INT TERM
git worktree add --detach -q "$WORK/src" "$SHA"
TARGET="${HUB_RESTART_TARGET_DIR:-${STATE_DIR}/target}"
mkdir -p "$TARGET"
log ""
log "== build (isolated worktree and target dir; nothing installed) =="
( cd "$WORK/src" && CARGO_TARGET_DIR="$TARGET" cargo build --release --locked --bins )

log ""
log "== version and hash comparison =="
CHANGED=0
for bin in "${BINARIES[@]}"; do
    new="$TARGET/release/$bin"
    [[ -x "$new" ]] || { err "build did not produce $new"; exit 1; }
    old="$BIN_DIR/$bin"
    if [[ -x "$old" ]]; then
        if [[ "$(sha_of "$new")" == "$(sha_of "$old")" ]]; then
            log "$bin: identical (sha256 $(sha_of "$new" | cut -c1-12))"
        else
            CHANGED=1
            log "$bin: installed  $(version_of "$old")"
            log "$bin: new        $(version_of "$new")"
        fi
    else
        CHANGED=1
        log "$bin: not installed; new $(version_of "$new")"
    fi
done
if [[ $CHANGED -eq 0 ]]; then
    log "Nothing differs from the installed binaries; a restart would deploy nothing."
fi

# ── backup (harmless, so it also runs in dry-run) ──────────────────────────
STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
BACKUP="$STATE_DIR/backup-$STAMP-${SHA:0:12}"
log ""
log "== backup of the installed binaries =="
mkdir -p "$BACKUP"
for bin in "${BINARIES[@]}"; do
    if [[ -x "$BIN_DIR/$bin" ]]; then
        cp --preserve=mode,timestamps "$BIN_DIR/$bin" "$BACKUP/$bin"
        [[ "$(sha_of "$BIN_DIR/$bin")" == "$(sha_of "$BACKUP/$bin")" ]] || { err "backup hash mismatch: $bin"; exit 1; }
    fi
done
log "saved to $BACKUP (hash-checked)"

# ── steps ──────────────────────────────────────────────────────────────────
log ""
log "== steps =="
BASELINE_GAP="$(store_gap)"
log "Baseline stream_length - pg_message_count gap before the restart: $BASELINE_GAP"
log "Announce on the machine-hygiene thread first, with the expected downtime (seconds)."
for bin in "${BINARIES[@]}"; do
    if [[ $APPLY -eq 1 ]]; then
        tmp="$(mktemp "$BIN_DIR/$bin.XXXXXX")"
        cp "$TARGET/release/$bin" "$tmp"
        chmod --reference="$BIN_DIR/$bin" "$tmp" 2>/dev/null || chmod 755 "$tmp"
        mv -f "$tmp" "$BIN_DIR/$bin"
        log "+ installed $bin atomically"
    else
        log "[dry-run] would install $TARGET/release/$bin over $BIN_DIR/$bin (temp file + rename in the same directory, so the running process keeps its old inode)"
    fi
done
restart_and_verify

log ""
log "== rollback =="
log "scripts/hub-restart.sh --rollback $BACKUP --apply"
log "(restores the three binaries from the hash-checked backup, restarts $UNIT, repeats the readback)"
if [[ $APPLY -eq 0 ]]; then
    log ""
    log "Dry run complete: no binary was installed and the service was not restarted."
fi
