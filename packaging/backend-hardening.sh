#!/usr/bin/env bash
# backend-hardening.sh -- opt-in, root-owned hardening of the SYSTEM Redis and PostgreSQL
# services behind an authoritative agent-bus hub (Linux, systemd).
#
# Why: on 2026-10-26 a power cut left 1155 bad bytes at the tail of the hub's Redis AOF.
# redis-server refused to load it, its packaged unit burned the 5-in-10 s start limit in 2 s,
# and the fleet bus stayed down until someone ran `redis-check-aof --fix` by hand. The same
# hub's PostgreSQL history had no copy at all. See packaging/README.md, "Backend hardening".
#
# Installs, all as root (through sudo unless already root):
#   Redis:      <libexec>/agent-bus-redis-aof-preflight
#               /etc/systemd/system/<redis-unit>.d/agent-bus-aof-preflight.conf
#               /etc/sysctl.d/60-agent-bus-redis-overcommit.conf (applied live, read back)
#   PostgreSQL: <libexec>/agent-bus-postgres-dump
#               <dump-dir> (owned by <pg-user>, mode 0700)
#               /etc/systemd/system/agent-bus-postgres-dump.{service,timer} (timer enabled)
# It NEVER starts, stops or restarts Redis or PostgreSQL. The drop-in takes effect at Redis's
# next start; the dump first runs when the timer fires.
#
# Usage:
#   packaging/backend-hardening.sh [OPTIONS]          (no build needed; the usual entry point)
#   ./install.sh --with-backend-hardening [--dry-run]  (after install.sh's build and deploy;
#                forwards --backend-<option> <value> as --<option> <value>)
#
# Options (defaults match the asuspro13 hub):
#   --dry-run                           Render everything and print every target file (with its
#                                       content) and every root command. Changes nothing and
#                                       never invokes sudo.
#   --only redis|postgres               Install one half only (default: both).
#   --libexec-dir <dir>                 Script directory (default: /usr/local/libexec/agent-bus).
#   --redis-unit <name>                 Redis unit to extend (default: redis-server.service).
#   --redis-aof-dir <dir>               AOF directory, Redis `dir` + `appenddirname`
#                                       (default: /var/lib/redis/appendonlydir).
#   --redis-manifest <name>             AOF manifest, `appendfilename` + .manifest
#                                       (default: appendonly.aof.manifest).
#   --redis-max-fix-bytes <n>           Largest bad tail repaired automatically (default: 1048576).
#   --redis-restart-sec <n>             RestartSec for the Redis unit (default: 5).
#   --redis-start-limit-interval-sec <n>  (default: 300)
#   --redis-start-limit-burst <n>       (default: 10). burst * restart-sec must fit the interval.
#   --redis-overcommit-memory <0|1|2>   vm.overcommit_memory (default: 1).
#   --redis-check-aof <path>            redis-check-aof the preflight runs and the drop-in's
#                                       ExecPaths= admits (default: found on PATH).
#   --allow-missing-aof                 Install even when the AOF manifest does not exist yet
#                                       (fresh install, AOF off). Otherwise a wrong path refuses,
#                                       because the preflight treats a missing manifest as clean.
#   --pg-service <name>                 Unit the dump is ordered after (default: postgresql.service;
#                                       Debian's postgresql@<ver>-<cluster> units sort before it).
#   --pg-user <name>                    Dump user, peer-authenticated (default: postgres).
#   --pg-port <n>                       Cluster port (default: 5300).
#   --pg-databases "<db> <db>..."       Databases to dump (default: "redis_backend workq").
#   --pg-dump-dir <dir>                 Dump directory (default: /var/backups/agent-bus-postgres).
#                                       Must not be under /tmp or /var/tmp (PrivateTmp, status 226).
#   --pg-keep <n>                       Dumps kept per database (default: 14).
#   --pg-on-calendar <spec>             Timer OnCalendar (default: "*-*-* 03:30:00").
#   --pg-randomized-delay-sec <n>       Timer RandomizedDelaySec (default: 900).
#   --root-dir <dir>                    Write every target under <dir> instead of /, as the
#                                       current user, without ownership changes, systemctl,
#                                       sysctl or host checks. For packaging and tests only.
#   --validate-only                     Check the options, then exit 0 without rendering.
#   -h, --help                          Show this help and exit.
#
# Environment: AGENT_BUS_HARDENING_SUDO overrides the privilege command (default: `sudo` when
# not root, nothing when root). Tests point it at a recorder so no real sudo can run.
#
# Exit codes: 0 success, 1 a step or host check failed, 2 invalid options.

set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"

DRY_RUN=0
ONLY=both
LIBEXEC_DIR=/usr/local/libexec/agent-bus
REDIS_UNIT=redis-server.service
REDIS_AOF_DIR=/var/lib/redis/appendonlydir
REDIS_MANIFEST=appendonly.aof.manifest
REDIS_MAX_FIX_BYTES=1048576
REDIS_RESTART_SEC=5
REDIS_START_LIMIT_INTERVAL_SEC=300
REDIS_START_LIMIT_BURST=10
REDIS_OVERCOMMIT_MEMORY=1
REDIS_CHECK_AOF=redis-check-aof
ALLOW_MISSING_AOF=0
PG_SERVICE=postgresql.service
PG_USER=postgres
PG_PORT=5300
PG_DATABASES="redis_backend workq"
PG_DUMP_DIR=/var/backups/agent-bus-postgres
PG_KEEP=14
PG_ON_CALENDAR="*-*-* 03:30:00"
PG_RANDOMIZED_DELAY_SEC=900
ROOT_DIR=""
VALIDATE_ONLY=0

log()  { printf '[backend-hardening] %s\n' "$*"; }
err()  { printf '[backend-hardening] ERROR: %s\n' "$*" >&2; }
usage() { sed -n '2,/^set -euo pipefail$/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'; }

need_value() { [[ $# -ge 2 ]] || { err "$1 requires a value"; exit 2; }; }

while [[ $# -gt 0 ]]; do
    case "$1" in
        --dry-run) DRY_RUN=1; shift ;;
        --allow-missing-aof) ALLOW_MISSING_AOF=1; shift ;;
        --validate-only) VALIDATE_ONLY=1; shift ;;
        -h | --help) usage; exit 0 ;;
        --only) need_value "$@"; ONLY="$2"; shift 2 ;;
        --libexec-dir) need_value "$@"; LIBEXEC_DIR="$2"; shift 2 ;;
        --redis-unit) need_value "$@"; REDIS_UNIT="$2"; shift 2 ;;
        --redis-aof-dir) need_value "$@"; REDIS_AOF_DIR="$2"; shift 2 ;;
        --redis-manifest) need_value "$@"; REDIS_MANIFEST="$2"; shift 2 ;;
        --redis-max-fix-bytes) need_value "$@"; REDIS_MAX_FIX_BYTES="$2"; shift 2 ;;
        --redis-restart-sec) need_value "$@"; REDIS_RESTART_SEC="$2"; shift 2 ;;
        --redis-start-limit-interval-sec) need_value "$@"; REDIS_START_LIMIT_INTERVAL_SEC="$2"; shift 2 ;;
        --redis-start-limit-burst) need_value "$@"; REDIS_START_LIMIT_BURST="$2"; shift 2 ;;
        --redis-overcommit-memory) need_value "$@"; REDIS_OVERCOMMIT_MEMORY="$2"; shift 2 ;;
        --redis-check-aof) need_value "$@"; REDIS_CHECK_AOF="$2"; shift 2 ;;
        --pg-service) need_value "$@"; PG_SERVICE="$2"; shift 2 ;;
        --pg-user) need_value "$@"; PG_USER="$2"; shift 2 ;;
        --pg-port) need_value "$@"; PG_PORT="$2"; shift 2 ;;
        --pg-databases) need_value "$@"; PG_DATABASES="$2"; shift 2 ;;
        --pg-dump-dir) need_value "$@"; PG_DUMP_DIR="$2"; shift 2 ;;
        --pg-keep) need_value "$@"; PG_KEEP="$2"; shift 2 ;;
        --pg-on-calendar) need_value "$@"; PG_ON_CALENDAR="$2"; shift 2 ;;
        --pg-randomized-delay-sec) need_value "$@"; PG_RANDOMIZED_DELAY_SEC="$2"; shift 2 ;;
        --root-dir) need_value "$@"; ROOT_DIR="$2"; shift 2 ;;
        *) err "Unknown argument: $1"; usage >&2; exit 2 ;;
    esac
done

# ── validation: every setting is checked before anything is rendered or run ──────────────
invalid=0
bad() { err "$*"; invalid=1; }
is_uint() { [[ "$1" =~ ^[0-9]+$ ]]; }
is_posint() { [[ "$1" =~ ^[0-9]+$ && "$1" -gt 0 ]]; }
# A path or value that lands in a unit's ExecStart/ReadWritePaths: systemd splits on
# whitespace, expands % specifiers and interprets quotes and backslashes, and the installer
# substitutes with sed, so only plain absolute paths pass.
is_plain_abs_path() { [[ "$1" =~ ^/[A-Za-z0-9._/@+-]*$ && "$1" != *"/../"* && "$1" != */.. ]]; }
is_unit_name() { [[ "$1" =~ ^[A-Za-z0-9@._:-]+\.service$ ]]; }

case "$ONLY" in both | redis | postgres) ;; *) bad "--only must be redis, postgres or both, got '$ONLY'" ;; esac
is_plain_abs_path "$LIBEXEC_DIR" || bad "--libexec-dir must be a plain absolute path, got '$LIBEXEC_DIR'"
if [[ -n "$ROOT_DIR" ]]; then
    is_plain_abs_path "$ROOT_DIR" || bad "--root-dir must be a plain absolute path, got '$ROOT_DIR'"
fi

if [[ "$ONLY" != postgres ]]; then
    is_unit_name "$REDIS_UNIT" || bad "--redis-unit must be a .service unit name, got '$REDIS_UNIT'"
    is_plain_abs_path "$REDIS_AOF_DIR" || bad "--redis-aof-dir must be a plain absolute path, got '$REDIS_AOF_DIR'"
    [[ "$REDIS_MANIFEST" =~ ^[A-Za-z0-9._-]+\.manifest$ ]] ||
        bad "--redis-manifest must be a file name ending in .manifest, got '$REDIS_MANIFEST'"
    is_posint "$REDIS_MAX_FIX_BYTES" || bad "--redis-max-fix-bytes must be a positive integer"
    is_posint "$REDIS_RESTART_SEC" || bad "--redis-restart-sec must be a positive integer"
    is_posint "$REDIS_START_LIMIT_INTERVAL_SEC" || bad "--redis-start-limit-interval-sec must be a positive integer"
    is_posint "$REDIS_START_LIMIT_BURST" || bad "--redis-start-limit-burst must be a positive integer"
    if is_posint "$REDIS_RESTART_SEC" && is_posint "$REDIS_START_LIMIT_INTERVAL_SEC" &&
        is_posint "$REDIS_START_LIMIT_BURST" &&
        ((REDIS_START_LIMIT_BURST * REDIS_RESTART_SEC > REDIS_START_LIMIT_INTERVAL_SEC)); then
        # Starts spaced restart-sec apart must be able to fill the burst inside the interval,
        # or systemd never reaches the limit and a deterministic failure loops forever.
        bad "--redis-start-limit-burst ($REDIS_START_LIMIT_BURST) x --redis-restart-sec ($REDIS_RESTART_SEC) exceeds --redis-start-limit-interval-sec ($REDIS_START_LIMIT_INTERVAL_SEC); the start limit could never fire"
    fi
    [[ "$REDIS_OVERCOMMIT_MEMORY" =~ ^[012]$ ]] || bad "--redis-overcommit-memory must be 0, 1 or 2"
    [[ "$REDIS_CHECK_AOF" == redis-check-aof ]] || is_plain_abs_path "$REDIS_CHECK_AOF" ||
        bad "--redis-check-aof must be a plain absolute path, got '$REDIS_CHECK_AOF'"
fi

if [[ "$ONLY" != redis ]]; then
    is_unit_name "$PG_SERVICE" || bad "--pg-service must be a .service unit name, got '$PG_SERVICE'"
    [[ "$PG_USER" =~ ^[a-z_][a-z0-9_-]*$ ]] || bad "--pg-user must be a plain user name, got '$PG_USER'"
    if ! is_posint "$PG_PORT" || ((PG_PORT > 65535)); then
        bad "--pg-port must be 1-65535, got '$PG_PORT'"
    fi
    is_posint "$PG_KEEP" || bad "--pg-keep must be a positive integer (the script refuses 0)"
    is_uint "$PG_RANDOMIZED_DELAY_SEC" || bad "--pg-randomized-delay-sec must be a non-negative integer"
    [[ "$PG_ON_CALENDAR" =~ ^[A-Za-z0-9\ :*/.,~-]+$ ]] || bad "--pg-on-calendar has unsupported characters: '$PG_ON_CALENDAR'"
    read -r -a pg_db_list <<<"$PG_DATABASES"
    ((${#pg_db_list[@]} > 0)) || bad "--pg-databases must name at least one database"
    for db in "${pg_db_list[@]}"; do
        [[ "$db" =~ ^[A-Za-z0-9_]+$ ]] || bad "database name must be [A-Za-z0-9_]+, got '$db'"
    done
    PG_DATABASES="${pg_db_list[*]}"
    if ! is_plain_abs_path "$PG_DUMP_DIR"; then
        bad "--pg-dump-dir must be a plain absolute path, got '$PG_DUMP_DIR'"
    elif [[ "$PG_DUMP_DIR" =~ ^/(var/)?tmp(/|$) ]]; then
        # The unit's PrivateTmp hides the host's /tmp and /var/tmp, so systemd fails it with
        # status 226 (namespace setup). Measured on asuspro13 2026-10-26.
        bad "--pg-dump-dir must not be under /tmp or /var/tmp (PrivateTmp makes the unit fail with status 226)"
    elif [[ "$PG_DUMP_DIR" == / ]]; then
        bad "--pg-dump-dir must not be /"
    fi
fi
((invalid == 0)) || exit 2
if ((VALIDATE_ONLY)); then
    exit 0
fi

if [[ "$(uname -s)" != Linux ]]; then
    err "backend hardening targets Linux systemd hosts only"
    exit 2
fi

# ── privilege and rendering ───────────────────────────────────────────────────────────────
if [[ -n "${AGENT_BUS_HARDENING_SUDO+set}" ]]; then
    SUDO="$AGENT_BUS_HARDENING_SUDO"
elif [[ "$(id -u)" -eq 0 ]]; then
    SUDO=""
else
    SUDO=sudo
fi
[[ -z "$ROOT_DIR" ]] || SUDO=""

# Print a root command; run it unless this is a dry run.
run_root() {
    local shown="$*"
    [[ -z "$SUDO" ]] || shown="$SUDO $shown"
    if ((DRY_RUN)); then
        log "  would run: $shown"
        return 0
    fi
    log "  run: $shown"
    if [[ -n "$SUDO" ]]; then
        "$SUDO" "$@"
    else
        "$@"
    fi
}

escape_sed_replacement() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//&/\\&}"
    value="${value//|/\\|}"
    printf '%s' "$value"
}

stage="$(mktemp -d)"
trap 'rm -rf -- "$stage"' EXIT

render() { # render <template> <output>
    local -a expr=()
    local key
    for key in START_LIMIT_INTERVAL_SEC START_LIMIT_BURST RESTART_SEC PREFLIGHT_BIN \
        PREFLIGHT_EXEC_PATHS REDIS_CHECK_AOF AOF_DIR \
        AOF_MANIFEST MAX_FIX_BYTES OVERCOMMIT_MEMORY PG_SERVICE PG_USER DUMP_BIN DUMP_DIR \
        PG_PORT DUMP_KEEP PG_DATABASES ON_CALENDAR RANDOMIZED_DELAY_SEC; do
        expr+=(-e "s|@${key}@|$(escape_sed_replacement "${values[$key]}")|g")
    done
    sed "${expr[@]}" "$1" >"$2"
    if grep -n '@[A-Z_]*@' "$2" >&2; then
        err "unrendered token in $2 (from $1)"
        exit 1
    fi
}

PREFLIGHT_BIN="$LIBEXEC_DIR/agent-bus-redis-aof-preflight"
DUMP_BIN="$LIBEXEC_DIR/agent-bus-postgres-dump"

# Redis's packaged unit sets NoExecPaths=/ and admits only redis-server, /usr/lib and /lib.
# The drop-in must add every file the preflight executes, or Redis fails its next start:
# the helper, the real /bin/sh named by its shebang, the checker, and each command on the
# helper's `# exec-deps:` line. Each is resolved to its real file here, on the target host.
find_exec() { # find_exec <command name or absolute path> -> prints the absolute path, unresolved
    local found
    if [[ "$1" == /* ]]; then
        found="$1"
    else
        found="$(command -v -- "$1" || true)"
    fi
    if [[ "$found" != /* || ! -x "$found" ]]; then
        err "the Redis preflight needs '$1', which is not an executable on this host"
        return 1
    fi
    if ! is_plain_abs_path "$found"; then
        err "'$1' is at '$found', which a unit file cannot hold"
        return 1
    fi
    printf '%s\n' "$found"
}
resolve_exec() { # resolve_exec <command name or absolute path> -> prints the real file's path
    local found real
    found="$(find_exec "$1")"
    real="$(readlink -f -- "$found")"
    if ! is_plain_abs_path "$real"; then
        err "'$1' resolves to '$real', which an ExecPaths= list cannot hold"
        return 1
    fi
    printf '%s\n' "$real"
}
PREFLIGHT_EXEC_PATHS=""
REDIS_CHECK_AOF_PIN=""
if [[ "$ONLY" != postgres ]]; then
    read -r -a preflight_deps <<<"$(sed -n 's/^# exec-deps: //p' "$SCRIPT_DIR/libexec/agent-bus-redis-aof-preflight")"
    if ((${#preflight_deps[@]} == 0)); then
        err "no '# exec-deps:' line in libexec/agent-bus-redis-aof-preflight"
        exit 1
    fi
    # Plain assignments, so set -e stops the script when a resolution fails.
    # Redis's binaries are one multi-call program that picks checker or server mode from the
    # name it was started under (upstream `make install` links redis-check-aof to
    # redis-server). So the helper is pinned to the checker by its own NAME, unresolved, and
    # only ExecPaths= gets the real file the kernel checks (claude review of PR #189).
    REDIS_CHECK_AOF_PIN="$(find_exec "$REDIS_CHECK_AOF")"
    checker_real="$(resolve_exec "$REDIS_CHECK_AOF_PIN")"
    sh_real="$(resolve_exec /bin/sh)"
    exec_list=("$PREFLIGHT_BIN" "$sh_real" "$checker_real")
    for dep in "${preflight_deps[@]}"; do
        dep_real="$(resolve_exec "$dep")"
        exec_list+=("$dep_real")
    done
    declare -A seen_exec=()
    for path in "${exec_list[@]}"; do
        [[ -n "${seen_exec[$path]+set}" ]] && continue
        seen_exec[$path]=1
        PREFLIGHT_EXEC_PATHS+="${PREFLIGHT_EXEC_PATHS:+ }$path"
    done
fi

declare -A values=(
    [START_LIMIT_INTERVAL_SEC]="$REDIS_START_LIMIT_INTERVAL_SEC"
    [START_LIMIT_BURST]="$REDIS_START_LIMIT_BURST"
    [RESTART_SEC]="$REDIS_RESTART_SEC"
    [PREFLIGHT_BIN]="$PREFLIGHT_BIN"
    [PREFLIGHT_EXEC_PATHS]="$PREFLIGHT_EXEC_PATHS"
    [REDIS_CHECK_AOF]="$REDIS_CHECK_AOF_PIN"
    [AOF_DIR]="$REDIS_AOF_DIR"
    [AOF_MANIFEST]="$REDIS_MANIFEST"
    [MAX_FIX_BYTES]="$REDIS_MAX_FIX_BYTES"
    [OVERCOMMIT_MEMORY]="$REDIS_OVERCOMMIT_MEMORY"
    [PG_SERVICE]="$PG_SERVICE"
    [PG_USER]="$PG_USER"
    [DUMP_BIN]="$DUMP_BIN"
    [DUMP_DIR]="$PG_DUMP_DIR"
    [PG_PORT]="$PG_PORT"
    [DUMP_KEEP]="$PG_KEEP"
    [PG_DATABASES]="$PG_DATABASES"
    [ON_CALENDAR]="$PG_ON_CALENDAR"
    [RANDOMIZED_DELAY_SEC]="$PG_RANDOMIZED_DELAY_SEC"
)

# Each entry: <staged source>|<target path>|<mode>|<reload: unit file that needs daemon-reload>
declare -a files=()
if [[ "$ONLY" != postgres ]]; then
    cp "$SCRIPT_DIR/libexec/agent-bus-redis-aof-preflight" "$stage/preflight"
    render "$SCRIPT_DIR/systemd/redis-server.service.d/agent-bus-aof-preflight.conf.in" "$stage/dropin"
    render "$SCRIPT_DIR/sysctl/60-agent-bus-redis-overcommit.conf.in" "$stage/sysctl"
    files+=(
        "$stage/preflight|$PREFLIGHT_BIN|0755|0"
        "$stage/dropin|/etc/systemd/system/${REDIS_UNIT}.d/agent-bus-aof-preflight.conf|0644|1"
        "$stage/sysctl|/etc/sysctl.d/60-agent-bus-redis-overcommit.conf|0644|0"
    )
fi
if [[ "$ONLY" != redis ]]; then
    cp "$SCRIPT_DIR/libexec/agent-bus-postgres-dump" "$stage/dump"
    render "$SCRIPT_DIR/systemd/agent-bus-postgres-dump.service.in" "$stage/service"
    render "$SCRIPT_DIR/systemd/agent-bus-postgres-dump.timer.in" "$stage/timer"
    files+=(
        "$stage/dump|$DUMP_BIN|0755|0"
        "$stage/service|/etc/systemd/system/agent-bus-postgres-dump.service|0644|1"
        "$stage/timer|/etc/systemd/system/agent-bus-postgres-dump.timer|0644|1"
    )
fi

# ── host checks (apply only; a dry run must not need sudo) ────────────────────────────────
log "agent-bus backend hardening (only=$ONLY, root-dir=${ROOT_DIR:-/})"
((DRY_RUN)) && log "DRY RUN: nothing is written and sudo is never invoked."
if ((DRY_RUN == 0)) && [[ -z "$ROOT_DIR" ]]; then
    if [[ "$ONLY" != postgres ]]; then
        if ! systemctl cat "$REDIS_UNIT" >/dev/null 2>&1; then
            err "systemd unit $REDIS_UNIT not found; pass --redis-unit"
            exit 1
        fi
        # The data directory is usually 0750 redis, so this needs root. A missing manifest
        # would make the preflight pass silently on every start, protecting nothing.
        if ! "${SUDO:-command}" test -f "$REDIS_AOF_DIR/$REDIS_MANIFEST"; then
            if ((ALLOW_MISSING_AOF)); then
                log "AOF manifest $REDIS_AOF_DIR/$REDIS_MANIFEST is absent; continuing (--allow-missing-aof)."
            else
                err "AOF manifest $REDIS_AOF_DIR/$REDIS_MANIFEST not found. Check Redis's dir, appenddirname and appendfilename, then pass --redis-aof-dir/--redis-manifest, or --allow-missing-aof if AOF is not on yet."
                exit 1
            fi
        fi
    fi
    if [[ "$ONLY" != redis ]] && ! getent passwd "$PG_USER" >/dev/null; then
        err "dump user $PG_USER does not exist; pass --pg-user"
        exit 1
    fi
elif ((DRY_RUN)) && [[ -z "$ROOT_DIR" ]]; then
    [[ "$ONLY" == postgres ]] || log "  would check: systemctl cat $REDIS_UNIT; ${SUDO:+$SUDO }test -f $REDIS_AOF_DIR/$REDIS_MANIFEST"
    [[ "$ONLY" == redis ]] || log "  would check: getent passwd $PG_USER"
fi

# ── files ─────────────────────────────────────────────────────────────────────────────────
reload=0
for entry in "${files[@]}"; do
    IFS='|' read -r src target mode needs_reload <<<"$entry"
    dest="${ROOT_DIR}${target}"
    if [[ -f "$dest" ]] && cmp -s "$src" "$dest"; then
        log "unchanged: $dest"
        # A dry run shows every file, so the operator can review what is already installed.
        ((DRY_RUN == 0)) || sed 's/^/      | /' "$src"
        continue
    fi
    state=new
    [[ ! -e "$dest" ]] || state=changed
    log "$state: $dest (mode $mode${ROOT_DIR:+, current user}${ROOT_DIR:-, root:root})"
    ((DRY_RUN == 0)) || sed 's/^/      | /' "$src"
    if [[ -n "$ROOT_DIR" ]]; then
        run_root install -D -m "$mode" "$src" "$dest"
    else
        run_root install -D -m "$mode" -o root -g root "$src" "$dest"
    fi
    ((needs_reload == 0)) || reload=1
done

if [[ "$ONLY" != redis ]]; then
    if [[ -n "$ROOT_DIR" ]]; then
        run_root install -d -m 0700 "${ROOT_DIR}${PG_DUMP_DIR}"
    else
        run_root install -d -m 0700 -o "$PG_USER" -g "$PG_USER" "$PG_DUMP_DIR"
    fi
fi

# ── activation: reload, sysctl, timer. Never a Redis or PostgreSQL start/stop/restart. ────
if [[ -n "$ROOT_DIR" ]]; then
    log "Skipping systemctl and sysctl (--root-dir)."
else
    if ((reload)); then
        run_root systemctl daemon-reload
    fi
    if [[ "$ONLY" != postgres ]]; then
        run_root sysctl -p /etc/sysctl.d/60-agent-bus-redis-overcommit.conf
        if ((DRY_RUN)); then
            log "  would check: sysctl -n vm.overcommit_memory = $REDIS_OVERCOMMIT_MEMORY"
        else
            live="$(sysctl -n vm.overcommit_memory)"
            if [[ "$live" != "$REDIS_OVERCOMMIT_MEMORY" ]]; then
                err "vm.overcommit_memory reads $live, not $REDIS_OVERCOMMIT_MEMORY, after applying the sysctl file"
                exit 1
            fi
            log "vm.overcommit_memory = $live"
        fi
    fi
    if [[ "$ONLY" != redis ]]; then
        # Starts the TIMER only; the dump itself first runs at the next OnCalendar time.
        run_root systemctl enable --now agent-bus-postgres-dump.timer
    fi
fi

[[ "$ONLY" == postgres ]] || log "Not restarting $REDIS_UNIT: the preflight drop-in applies at its next start. Check with: systemctl show $REDIS_UNIT -p DropInPaths,ExecStartPre,RestartUSec"
[[ "$ONLY" == redis ]] || log "Not touching $PG_SERVICE. Test a dump now with: sudo systemctl start agent-bus-postgres-dump.service && journalctl -u agent-bus-postgres-dump.service -n 20"
if ((DRY_RUN)); then
    log "Dry run complete; nothing was changed."
else
    log "Backend hardening installed."
fi
