#!/usr/bin/env bash
# Owned Linux fixture for the root-owned Redis/PostgreSQL hardening that
# `install.sh --with-backend-hardening` installs (packaging/README.md, "Backend hardening").
#
# Nothing here needs root, Redis, PostgreSQL or systemd, and nothing touches the host:
# fake redis-check-aof/pg_dump/pg_restore replay the formats and exit codes measured from
# Redis 7.0.15 and PostgreSQL 16 on asuspro13 (2026-10-26), and every privileged installer
# command goes to a recorder through AGENT_BUS_HARDENING_SUDO, which never executes it.
#
#   bash packaging/tests/backend-hardening-test.sh
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
preflight="${repo_root}/packaging/libexec/agent-bus-redis-aof-preflight"
dump="${repo_root}/packaging/libexec/agent-bus-postgres-dump"
hardening="${repo_root}/packaging/backend-hardening.sh"

# Always under /tmp, whatever TMPDIR says, so the guard below can allow the cleanup rm -rf.
test_root="$(mktemp -d /tmp/agent-bus-backend-hardening.XXXXXX)"
case "$test_root" in
    /tmp/* | /var/tmp/*) ;;
    *) printf 'unsafe temporary directory: %s\n' "$test_root" >&2; exit 1 ;;
esac
trap 'rm -rf -- "$test_root"' EXIT

fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

# ── fakes ──────────────────────────────────────────────────────────────────────────────────
fake_bin="${test_root}/fake-bin"
mkdir -p "$fake_bin"

# redis-check-aof: reads ./state in the AOF directory: "valid", "bad <n>", "nocount" or
# "stuck <n>" (--fix "succeeds" but leaves the file invalid). Each --fix is logged to ./fixlog.
# Shell builtins only, so it still runs when a test limits PATH to the preflight's exec-deps.
# The "AOF analyzed ... diff=" lines are the Redis 7.0.15 report format.
cat >"${fake_bin}/redis-check-aof" <<'EOF'
#!/bin/sh
IFS= read -r state <state || :
if [ "$1" = "--fix" ]; then
    read -r answer
    echo "fix $answer" >> fixlog
    case $state in bad*) echo valid > state ;; esac
    exit 0
fi
case $state in
    valid) exit 0 ;;
    nocount) echo "Invalid AOF manifest file format"; exit 1 ;;
esac
n=${state#* }
echo "AOF analyzed: filename=appendonly.aof.1.base.rdb, size=10, ok_up_to=10, ok_up_to_line=1, diff=0"
echo "AOF appendonly.aof.1.incr.aof format error"
echo "AOF analyzed: filename=appendonly.aof.1.incr.aof, size=900, ok_up_to=800, ok_up_to_line=9, diff=$n"
exit 1
EOF

# pg_dump: args are -p PORT -Fc -f FILE DB, logged to $DUMP_CALLS. A database listed in
# $DUMP_FAIL_DUMP exits 1 after writing half a file; one in $DUMP_FAIL_LIST writes a file
# that pg_restore --list rejects.
cat >"${fake_bin}/pg_dump" <<'EOF'
#!/bin/sh
out=$5; db=$6
echo "$*" >> "$DUMP_CALLS"
if grep -qx "$db" "$DUMP_FAIL_DUMP" 2>/dev/null; then echo half > "$out"; exit 1; fi
if grep -qx "$db" "$DUMP_FAIL_LIST" 2>/dev/null; then echo garbage > "$out"; exit 0; fi
echo "PGDMP $db" > "$out"
EOF
cat >"${fake_bin}/pg_restore" <<'EOF'
#!/bin/sh
grep -q '^PGDMP ' "$2"
EOF

# Privilege recorder: logs the command it was asked to run as root and runs NOTHING.
# `test` (the installer's AOF-manifest probe) answers with $RECORDER_TEST_RC.
cat >"${fake_bin}/sudo-recorder" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$RECORDER_LOG"
if [ "$1" = test ]; then exit "${RECORDER_TEST_RC:-0}"; fi
exit 0
EOF
# Unprivileged host probes the installer makes directly.
cat >"${fake_bin}/systemctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$SYSTEMCTL_LOG"
EOF
cat >"${fake_bin}/sysctl" <<'EOF'
#!/bin/sh
printf '%s\n' "$*" >> "$SYSTEMCTL_LOG"
[ "$1" != -n ] || echo "${FAKE_OVERCOMMIT:-1}"
EOF
cat >"${fake_bin}/getent" <<'EOF'
#!/bin/sh
[ "$1 $2" = "passwd postgres" ] && echo "postgres:x:117:125::/var/lib/postgresql:/bin/bash"
EOF
chmod 755 "${fake_bin}"/*

export AGENT_BUS_REDIS_CHECK_AOF="${fake_bin}/redis-check-aof"
export AGENT_BUS_PG_DUMP="${fake_bin}/pg_dump"
export AGENT_BUS_PG_RESTORE="${fake_bin}/pg_restore"
export DUMP_CALLS="${test_root}/dump-calls"
export DUMP_FAIL_DUMP="${test_root}/fail-dump"
export DUMP_FAIL_LIST="${test_root}/fail-list"
export RECORDER_LOG="${test_root}/recorder.log"
export SYSTEMCTL_LOG="${test_root}/systemctl.log"

# ── helpers ────────────────────────────────────────────────────────────────────────────────
new_aof() { # new_aof <name> <state> -> prints the AOF directory
    local dir="${test_root}/redis-$1/appendonlydir"
    mkdir -p "$dir"
    printf 'file appendonly.aof.1.base.rdb seq 1 type b\n' >"${dir}/appendonly.aof.manifest"
    printf 'original-bytes' >"${dir}/appendonly.aof.1.incr.aof"
    printf '%s' "$2" >"${dir}/state"
    printf '%s' "$dir"
}
run_preflight() { # run_preflight <dir> [limit] -> sets rc and out
    rc=0
    out="$(sh "$preflight" "$1" appendonly.aof.manifest "${2-1048576}" 2>&1)" || rc=$?
}
backups_of() { # backups_of <aof dir> -> sorted backup names, one per line
    find "$(dirname "$1")" -maxdepth 1 -name "$(basename "$1").preflight-backup.*" -printf '%f\n' | sort
}
run_dump() { # run_dump <args...> -> sets rc and out
    rc=0
    out="$(sh "$dump" "$@" 2>&1)" || rc=$?
}
preflight_exec_deps() { sed -n 's/^# exec-deps: //p' "$preflight"; }
dumps_in() { find "$1" -mindepth 1 -maxdepth 1 -printf '%f\n' | sort; }
run_hardening() { # run_hardening <args...> -> sets rc and out; privileged steps are recorded only
    rc=0
    : >"$RECORDER_LOG"
    : >"$SYSTEMCTL_LOG"
    out="$(PATH="${fake_bin}:${PATH}" AGENT_BUS_HARDENING_SUDO="${fake_bin}/sudo-recorder" \
        bash "$hardening" "$@" 2>&1)" || rc=$?
}

# ── Redis AOF preflight ───────────────────────────────────────────────────────────────────

# Protects: a clean Redis start stays a plain start, with no repair and no backup copy.
# Detects: a preflight that rewrites or copies a valid AOF on every start.
# Needs: sh, sed, awk and the fake redis-check-aof only.
# Breadcrumb: packaging/libexec/agent-bus-redis-aof-preflight
test_preflight_valid_aof_starts_without_repair() {
    local dir
    dir="$(new_aof valid valid)"
    run_preflight "$dir"
    [[ $rc -eq 0 ]] || fail "valid AOF refused: $out"
    [[ ! -e "${dir}/fixlog" ]] || fail "valid AOF was repaired"
    [[ -z "$(backups_of "$dir")" ]] || fail "valid AOF was copied"
    printf 'preflight-valid-aof-starts-without-repair: PASS\n'
}
test_preflight_valid_aof_starts_without_repair

# Protects: recovery from the 2026-10-26 power cut shape (1155 bad tail bytes) without
# losing the original: the AOF directory is copied aside BEFORE --fix truncates it.
# Detects: a repair with no backup, or a backup taken after the repair.
# Needs: sh, sed, awk, cp and the fake redis-check-aof only.
# Breadcrumb: packaging/libexec/agent-bus-redis-aof-preflight
test_preflight_small_tail_backed_up_before_fix() {
    local dir backup
    dir="$(new_aof small 'bad 1155')"
    run_preflight "$dir"
    [[ $rc -eq 0 ]] || fail "small bad tail not repaired: $out"
    [[ "$out" == *"1155 bad tail bytes"* ]] || fail "repair not reported: $out"
    [[ "$(cat "${dir}/fixlog")" == "fix y" ]] || fail "--fix not run exactly once"
    [[ "$(backups_of "$dir")" == appendonlydir.preflight-backup.1 ]] || fail "expected one backup"
    backup="$(dirname "$dir")/appendonlydir.preflight-backup.1"
    [[ "$(cat "${backup}/state")" == "bad 1155" ]] || fail "backup taken after the repair"
    [[ ! -e "${backup}/fixlog" ]] || fail "backup taken after --fix"
    [[ "$(cat "${backup}/appendonly.aof.1.incr.aof")" == original-bytes ]] || fail "backup lost bytes"
    printf 'preflight-small-tail-backed-up-before-fix: PASS\n'
}
test_preflight_small_tail_backed_up_before_fix

# Protects: real history. A bad region above the limit is not an interrupted write, and
# truncating it would discard data, so the start is refused and nothing is touched.
# Detects: a missing or ignored size limit. The same corruption at the limit is repaired in
# the same fixture (positive control), so the refusal is the limit and not a broken checker.
# Needs: sh, sed, awk and the fake redis-check-aof only.
# Breadcrumb: packaging/libexec/agent-bus-redis-aof-preflight
test_preflight_large_region_refuses_untouched() {
    local dir
    dir="$(new_aof large 'bad 5000')"
    run_preflight "$dir" 4999
    [[ $rc -eq 1 ]] || fail "large bad region not refused (rc=$rc)"
    [[ "$out" == *"above the 4999-byte repair limit"* ]] || fail "refusal not explained: $out"
    [[ ! -e "${dir}/fixlog" ]] || fail "large bad region was repaired"
    [[ -z "$(backups_of "$dir")" ]] || fail "refusal still copied the AOF"
    run_preflight "$dir" 5000
    [[ $rc -eq 0 ]] || fail "positive control: same corruption at the limit not repaired"
    printf 'preflight-large-region-refuses-untouched: PASS\n'
}
test_preflight_large_region_refuses_untouched

# Protects: no repair is attempted when the checker's report cannot be read.
# Detects: a parser that treats an unknown report as repairable or as clean.
# Needs: sh, sed, awk and the fake redis-check-aof only.
# Breadcrumb: packaging/libexec/agent-bus-redis-aof-preflight
test_preflight_report_without_count_refuses() {
    local dir
    dir="$(new_aof nocount nocount)"
    run_preflight "$dir"
    [[ $rc -eq 1 ]] || fail "uncounted failure not refused (rc=$rc)"
    [[ "$out" == *"without a byte count"* ]] || fail "refusal not explained: $out"
    [[ ! -e "${dir}/fixlog" ]] || fail "uncounted failure was repaired"
    printf 'preflight-report-without-count-refuses: PASS\n'
}
test_preflight_report_without_count_refuses

# Protects: Redis never starts on an AOF that is still invalid after --fix.
# Detects: trusting --fix's exit status instead of re-checking the file.
# Needs: sh, sed, awk and the fake redis-check-aof only.
# Breadcrumb: packaging/libexec/agent-bus-redis-aof-preflight
test_preflight_still_invalid_after_fix_refuses() {
    local dir
    dir="$(new_aof stuck 'stuck 10')"
    run_preflight "$dir"
    [[ $rc -eq 1 ]] || fail "still-invalid AOF allowed to start (rc=$rc)"
    [[ "$out" == *"still invalid after --fix"* ]] || fail "refusal not explained: $out"
    [[ "$(backups_of "$dir")" == appendonlydir.preflight-backup.1 ]] || fail "backup missing"
    printf 'preflight-still-invalid-after-fix-refuses: PASS\n'
}
test_preflight_still_invalid_after_fix_refuses

# Protects: a fresh install or an AOF-off Redis still starts (no manifest is not corruption).
# Detects: a preflight that blocks the first start of a new hub.
# Needs: sh only; the checker is never called.
# Breadcrumb: packaging/backend-hardening.sh (its --allow-missing-aof guard)
test_preflight_missing_manifest_is_clean_start() {
    run_preflight "${test_root}/absent/appendonlydir"
    [[ $rc -eq 0 ]] || fail "missing manifest refused the start (rc=$rc)"
    [[ "$out" == *"nothing to check"* ]] || fail "missing manifest not reported: $out"
    printf 'preflight-missing-manifest-is-clean-start: PASS\n'
}
test_preflight_missing_manifest_is_clean_start

# Protects: the repair limit is a number; a malformed drop-in argument never repairs.
# Detects: a limit like 1MB or -1 being compared as text or as zero.
# Needs: sh and the fake redis-check-aof only.
# Breadcrumb: packaging/systemd/redis-server.service.d/agent-bus-aof-preflight.conf.in
test_preflight_malformed_limit_is_usage_error() {
    local dir limit
    dir="$(new_aof limit 'bad 10')"
    for limit in '' 1MB -1; do
        run_preflight "$dir" "$limit"
        [[ $rc -eq 2 ]] || fail "limit '$limit' gave rc=$rc, want 2"
    done
    [[ ! -e "${dir}/fixlog" ]] || fail "malformed limit still repaired"
    printf 'preflight-malformed-limit-is-usage-error: PASS\n'
}
test_preflight_malformed_limit_is_usage_error

# Protects: disk space. Each repair adds a full AOF copy (113 MB on asuspro13), so only the
# newest three are kept, newest in .1.
# Detects: unbounded backup growth, or rotation that drops the newest copy.
# Needs: sh, sed, awk, cp, mv and the fake redis-check-aof only.
# Breadcrumb: packaging/libexec/agent-bus-redis-aof-preflight
test_preflight_backups_rotate_keep_three() {
    local dir generation parent
    dir="$(new_aof rotate 'bad 1')"
    parent="$(dirname "$dir")"
    for generation in 1 2 3 4; do
        printf 'bad %s' "$generation" >"${dir}/state"
        run_preflight "$dir"
        [[ $rc -eq 0 ]] || fail "repair $generation failed: $out"
    done
    [[ "$(backups_of "$dir" | tr '\n' ' ')" == "appendonlydir.preflight-backup.1 appendonlydir.preflight-backup.2 appendonlydir.preflight-backup.3 " ]] ||
        fail "rotation kept: $(backups_of "$dir" | tr '\n' ' ')"
    [[ "$(cat "${parent}/appendonlydir.preflight-backup.1/state")" == "bad 4" ]] || fail ".1 is not the newest"
    [[ "$(cat "${parent}/appendonlydir.preflight-backup.3/state")" == "bad 2" ]] || fail ".3 is not the oldest kept"
    printf 'preflight-backups-rotate-keep-three: PASS\n'
}
test_preflight_backups_rotate_keep_three

# Protects: Redis starting at all once the drop-in is installed. Ubuntu's redis-server unit
# sets NoExecPaths=/, so the preflight can run only the files the drop-in adds to ExecPaths=,
# and the installer adds the helper's `# exec-deps:` commands. An undeclared command would be
# "Permission denied" inside the unit and fail every Redis start.
# Detects: the preflight using a command missing from its exec-deps line. A PATH without `cp`
# must fail the same repair, which proves the restricted PATH is what the run used.
# Needs: sh, the exec-deps commands and the fake redis-check-aof (shell builtins only).
# Breadcrumb: packaging/libexec/agent-bus-redis-aof-preflight (exec-deps line)
test_preflight_runs_with_only_declared_exec_deps() {
    local dir sh_real restricted short dep generation
    sh_real="$(readlink -f /bin/sh)"
    restricted="${test_root}/exec-deps-path"
    short="${test_root}/exec-deps-path-no-cp"
    mkdir -p "$restricted" "$short"
    for dep in $(preflight_exec_deps); do
        ln -s "$(command -v "$dep")" "${restricted}/${dep}"
        [[ "$dep" == cp ]] || ln -s "$(command -v "$dep")" "${short}/${dep}"
    done
    [[ -L "${restricted}/cp" ]] || fail "exec-deps line does not declare cp"
    dir="$(new_aof exec-deps 'bad 1')"
    for generation in 1 2 3 4; do  # four repairs also exercise the rm -rf of .3
        printf 'bad %s' "$generation" >"${dir}/state"
        rc=0
        out="$(PATH="$restricted" "$sh_real" "$preflight" "$dir" appendonly.aof.manifest 1048576 2>&1)" || rc=$?
        [[ $rc -eq 0 ]] || fail "repair $generation needs an undeclared command: $out"
    done
    [[ "$(backups_of "$dir" | wc -l)" -eq 3 ]] || fail "restricted run did not rotate three backups"
    dir="$(new_aof exec-deps-control 'bad 1155')"
    rc=0
    out="$(PATH="$short" "$sh_real" "$preflight" "$dir" appendonly.aof.manifest 1048576 2>&1)" || rc=$?
    [[ $rc -ne 0 ]] || fail "control: the repair succeeded without cp on PATH, so PATH was not restricted"
    printf 'preflight-runs-with-only-declared-exec-deps: PASS\n'
}
test_preflight_runs_with_only_declared_exec_deps

# Protects: the repair path under Redis's own seccomp filter. Ubuntu's redis-server unit sets
# SystemCallFilter=~@privileged @resources, and @privileged includes chown, so a `cp -a`
# backup is killed with SIGSYS (measured 2026-10-27: code=dumped/status=SYS).
# Detects: the preflight making a syscall that filter kills, on the clean, repair and
# rotation paths. Control: `cp -a` must die under the same filter, or the host does not
# enforce it and the case reports SKIP instead of a PASS it could not earn.
# Needs: a working `systemd-run --user` (absent in CI containers: SKIP). NoExecPaths= is not
# exercised here: a user manager applies it only with unprivileged user namespaces.
# Breadcrumb: /usr/lib/systemd/system/redis-server.service (Ubuntu 24.04, Redis 7.0.15)
test_preflight_survives_redis_syscall_filter() {
    local -a filter=(-p SystemCallArchitectures=native -p 'SystemCallFilter=@system-service'
        -p 'SystemCallFilter=~@privileged @resources' -p NoNewPrivileges=yes)
    local dir generation probe
    if ! command -v systemd-run >/dev/null 2>&1 ||
        ! systemd-run --user --wait --pipe --quiet true >/dev/null 2>&1; then
        printf 'preflight-survives-redis-syscall-filter: SKIP (no systemd-run --user here)\n'
        return 0
    fi
    probe="${test_root}/seccomp-probe"
    mkdir -p "${probe}/src"
    printf 'x' >"${probe}/src/f"
    if systemd-run --user --wait --pipe --quiet "${filter[@]}" \
        "$(command -v cp)" -a "${probe}/src" "${probe}/dst" >/dev/null 2>&1; then
        printf 'preflight-survives-redis-syscall-filter: SKIP (filter not enforced: cp -a survived)\n'
        return 0
    fi
    dir="$(new_aof seccomp 'valid')"
    for generation in valid 1 2 3 4; do
        if [[ "$generation" == valid ]]; then printf 'valid' >"${dir}/state"; else printf 'bad %s' "$generation" >"${dir}/state"; fi
        rc=0
        out="$(systemd-run --user --wait --pipe --quiet "${filter[@]}" \
            --setenv=AGENT_BUS_REDIS_CHECK_AOF="${fake_bin}/redis-check-aof" \
            "$(readlink -f /bin/sh)" "$preflight" "$dir" appendonly.aof.manifest 1048576 2>&1)" || rc=$?
        [[ $rc -eq 0 ]] || fail "preflight ($generation) failed under the Redis syscall filter: $out"
    done
    [[ "$(backups_of "$dir" | wc -l)" -eq 3 ]] || fail "filtered repairs did not rotate three backups"
    [[ "$(cat "${dir}/state")" == valid ]] || fail "filtered repair did not run --fix"
    printf 'preflight-survives-redis-syscall-filter: PASS\n'
}
test_preflight_survives_redis_syscall_filter

# ── PostgreSQL dump ───────────────────────────────────────────────────────────────────────

# Protects: a restorable dump per database, written only after pg_restore can list it.
# Detects: the wrong port or format, or a dump left under its .partial name.
# Needs: sh and the fake pg_dump/pg_restore only.
# Breadcrumb: packaging/libexec/agent-bus-postgres-dump
test_dump_each_database_verified() {
    local dest="${test_root}/dump-each" db name
    : >"$DUMP_CALLS"
    run_dump "$dest" 5300 3 redis_backend workq
    [[ $rc -eq 0 ]] || fail "dump failed: $out"
    for db in redis_backend workq; do
        name="$(dumps_in "${dest}/${db}")"
        [[ "$name" =~ ^${db}\.[0-9]{8}T[0-9]{6}Z\.dump$ ]] || fail "$db: unexpected files: $name"
        [[ "$(cat "${dest}/${db}/${name}")" == "PGDMP ${db}" ]] || fail "$db: wrong content"
    done
    [[ "$(head -n1 "$DUMP_CALLS")" == "-p 5300 -Fc -f "* ]] || fail "pg_dump args: $(head -n1 "$DUMP_CALLS")"
    printf 'dump-each-database-verified: PASS\n'
}
test_dump_each_database_verified

# Protects: one bad database never stops the others and never replaces a good older dump;
# the run still exits 1 so the unit lands in systemctl --failed.
# Detects: a truncated or unlistable dump kept as good, a stray .partial, or exit 0.
# Needs: sh and the fake pg_dump/pg_restore only.
# Breadcrumb: packaging/systemd/agent-bus-postgres-dump.service.in
test_dump_bad_dump_discarded_others_continue() {
    local failure dest kept=redis_backend.20261025T033000Z.dump
    for failure in "$DUMP_FAIL_DUMP" "$DUMP_FAIL_LIST"; do
        dest="${test_root}/dump-bad-$(basename "$failure")"
        mkdir -p "${dest}/redis_backend"
        printf 'PGDMP old\n' >"${dest}/redis_backend/${kept}"
        printf 'redis_backend\n' >"$failure"
        run_dump "$dest" 5300 1 redis_backend workq
        rm -f -- "$failure"
        [[ $rc -eq 1 ]] || fail "$(basename "$failure"): rc=$rc, want 1"
        [[ "$out" == *"redis_backend: FAILED"* ]] || fail "failure not named: $out"
        [[ "$out" == *"1 of 2 databases failed"* ]] || fail "failure not counted: $out"
        [[ "$(dumps_in "${dest}/redis_backend")" == "$kept" ]] || fail "older good dump not kept alone"
        [[ "$(dumps_in "${dest}/workq" | wc -l)" -eq 1 ]] || fail "workq not dumped after the failure"
    done
    printf 'dump-bad-dump-discarded-others-continue: PASS\n'
}
test_dump_bad_dump_discarded_others_continue

# Protects: retention keeps the newest <keep> dumps, including the one just written, and
# never deletes files it did not write.
# Detects: deleting the new dump, keeping too many, or removing operator files.
# Needs: sh, sort and awk and the fake pg_dump/pg_restore only.
# Breadcrumb: packaging/libexec/agent-bus-postgres-dump
test_dump_retention_keeps_newest_ignores_other_files() {
    local dest="${test_root}/dump-retention" day names
    mkdir -p "${dest}/workq"
    for day in 21 22 23 24; do
        printf 'PGDMP old\n' >"${dest}/workq/workq.202610${day}T033000Z.dump"
    done
    printf 'operator note\n' >"${dest}/workq/notes.txt"
    run_dump "$dest" 5300 2 workq
    [[ $rc -eq 0 ]] || fail "dump failed: $out"
    names="$(dumps_in "${dest}/workq")"
    [[ "$names" == *notes.txt* ]] || fail "operator file removed"
    [[ "$(grep -c '\.dump$' <<<"$names")" -eq 2 ]] || fail "kept: $names"
    [[ "$names" == *workq.20261024T033000Z.dump* ]] || fail "newest older dump removed"
    [[ "$(grep '\.dump$' <<<"$names" | sort | tail -n1)" > workq.20261024T033000Z.dump ]] ||
        fail "the dump just written was removed"
    printf 'dump-retention-keeps-newest-ignores-other-files: PASS\n'
}
test_dump_retention_keeps_newest_ignores_other_files

# Protects: argument errors stop before pg_dump runs; a database name cannot escape its
# directory or widen the retention pattern.
# Detects: missing databases, a non-numeric port, keep=0, ../ or a space in a name.
# Needs: sh and the fake pg_dump only.
# Breadcrumb: packaging/backend-hardening.sh (validates the same settings at install)
test_dump_bad_arguments_usage_errors_before_dump() {
    local -a cases=('5300 3' 'x 3 workq' '5300 0 workq' '5300 3 ../etc' '5300 3 work-q')
    local args
    : >"$DUMP_CALLS"
    for args in "${cases[@]}"; do
        # shellcheck disable=SC2086 # word splitting builds the argument list on purpose
        run_dump "${test_root}/dump-args" $args
        [[ $rc -eq 2 ]] || fail "args '$args' gave rc=$rc, want 2"
    done
    run_dump "${test_root}/dump-args" 5300 3 'work q'
    [[ $rc -eq 2 ]] || fail "a space in a database name gave rc=$rc"
    [[ ! -s "$DUMP_CALLS" ]] || fail "pg_dump ran despite bad arguments"
    printf 'dump-bad-arguments-usage-errors-before-dump: PASS\n'
}
test_dump_bad_arguments_usage_errors_before_dump

# ── installer ─────────────────────────────────────────────────────────────────────────────

# Protects: the operator sees every root-owned file (with its rendered content) and every
# root command before anything happens, and a dry run never calls sudo.
# Detects: a target or command missing from the preview, an unrendered @TOKEN@, or sudo use.
# Needs: bash and the recorder fake; reads, but never writes, the host's /etc paths.
# Breadcrumb: packaging/README.md
test_installer_dry_run_shows_every_file_and_command() {
    local expected
    run_hardening --dry-run
    [[ $rc -eq 0 ]] || fail "dry run failed: $out"
    [[ ! -s "$RECORDER_LOG" ]] || fail "dry run invoked sudo: $(cat "$RECORDER_LOG")"
    for expected in \
        /usr/local/libexec/agent-bus/agent-bus-redis-aof-preflight \
        /etc/systemd/system/redis-server.service.d/agent-bus-aof-preflight.conf \
        /etc/sysctl.d/60-agent-bus-redis-overcommit.conf \
        /usr/local/libexec/agent-bus/agent-bus-postgres-dump \
        /etc/systemd/system/agent-bus-postgres-dump.service \
        /etc/systemd/system/agent-bus-postgres-dump.timer \
        'ExecStartPre=/usr/local/libexec/agent-bus/agent-bus-redis-aof-preflight /var/lib/redis/appendonlydir appendonly.aof.manifest 1048576' \
        'ExecPaths=/usr/local/libexec/agent-bus/agent-bus-redis-aof-preflight ' \
        "Environment=AGENT_BUS_REDIS_CHECK_AOF=$(readlink -f "${fake_bin}/redis-check-aof")" \
        'ExecStart=/usr/local/libexec/agent-bus/agent-bus-postgres-dump /var/backups/agent-bus-postgres 5300 14 redis_backend workq' \
        'vm.overcommit_memory = 1' \
        'install -d -m 0700 -o postgres -g postgres /var/backups/agent-bus-postgres' \
        'sysctl -p /etc/sysctl.d/60-agent-bus-redis-overcommit.conf' \
        'systemctl enable --now agent-bus-postgres-dump.timer' \
        'Dry run complete; nothing was changed.'; do
        [[ "$out" == *"$expected"* ]] || fail "dry run does not show: $expected"
    done
    if grep -E '@[A-Z_]+@' <<<"$out"; then fail "dry run shows an unrendered token"; fi
    printf 'installer-dry-run-shows-every-file-and-command: PASS\n'
}
test_installer_dry_run_shows_every_file_and_command

# Protects: Redis starting after install, without widening its exec allowlist further than
# the preflight needs. The packaged unit's NoExecPaths=/ admits only redis-server, /usr/lib
# and /lib; the drop-in adds exactly the helper, the real /bin/sh, the checker it pins and the
# real file of each exec-deps command (codex review of PR #189, P1).
# Detects: a missing entry (Redis cannot start) or an extra one (a wider shell surface), and
# a checker pin that differs from the admitted checker.
# Needs: bash, the recorder and the fake redis-check-aof on PATH; nothing runs as root.
# Breadcrumb: packaging/systemd/redis-server.service.d/agent-bus-aof-preflight.conf.in
test_installer_exec_paths_admit_exactly_the_preflight() {
    local line got want dep checker
    run_hardening --dry-run --only redis --allow-missing-aof
    [[ $rc -eq 0 ]] || fail "redis dry run failed: $out"
    # The dry run prints each rendered file as "| <line>"; the helper's own comments also say
    # ExecPaths=, so take the unit directive, which starts the line.
    line="$(sed -n 's/^ *| \(ExecPaths=\/.*\)$/\1/p' <<<"$out" | head -1)"
    [[ -n "$line" ]] || fail "drop-in has no ExecPaths= line"
    checker="$(readlink -f "${fake_bin}/redis-check-aof")"
    want="/usr/local/libexec/agent-bus/agent-bus-redis-aof-preflight"$'\n'"$(readlink -f /bin/sh)"$'\n'"$checker"
    for dep in $(preflight_exec_deps); do
        want+=$'\n'"$(readlink -f "$(command -v "$dep")")"
    done
    want="$(sort -u <<<"$want")"
    got="$(tr ' ' '\n' <<<"${line#ExecPaths=}" | sort -u)"
    [[ "$got" == "$want" ]] || fail "ExecPaths= admits [$(tr '\n' ' ' <<<"$got")], expected [$(tr '\n' ' ' <<<"$want")]"
    [[ "$out" == *"Environment=AGENT_BUS_REDIS_CHECK_AOF=${checker}"* ]] || fail "checker pin differs from the admitted checker"
    run_hardening --dry-run --only redis --allow-missing-aof --redis-check-aof "${test_root}/no-such-checker"
    [[ $rc -eq 1 && "$out" == *"not an executable on this host"* ]] ||
        fail "a missing checker was not refused (rc=$rc): $out"
    printf 'installer-exec-paths-admit-exactly-the-preflight: PASS\n'
}
test_installer_exec_paths_admit_exactly_the_preflight

# Protects: installing the hardening never starts, stops or restarts Redis or PostgreSQL
# (the drop-in waits for Redis's next start; only the dump TIMER is started).
# Detects: any privileged command outside the allowed install/reload/sysctl/timer set. The
# required sysctl and timer commands are asserted present, so a recorder that saw nothing
# cannot pass.
# Needs: bash and the recorder, systemctl, sysctl and getent fakes; runs nothing as root.
# Breadcrumb: packaging/backend-hardening.sh
test_installer_never_restarts_backend_services() {
    local line
    run_hardening
    [[ $rc -eq 0 ]] || fail "apply with recorder failed: $out"
    grep -qx 'sysctl -p /etc/sysctl.d/60-agent-bus-redis-overcommit.conf' "$RECORDER_LOG" ||
        fail "sysctl not applied"
    grep -qx 'systemctl enable --now agent-bus-postgres-dump.timer' "$RECORDER_LOG" ||
        fail "dump timer not enabled"
    while IFS= read -r line; do
        case "$line" in
            'test -f /var/lib/redis/appendonlydir/appendonly.aof.manifest') ;;
            'install -D -m 0755 -o root -g root '*' /usr/local/libexec/agent-bus/agent-bus-'*) ;;
            'install -D -m 0644 -o root -g root '*' /etc/'*) ;;
            'install -d -m 0700 -o postgres -g postgres /var/backups/agent-bus-postgres') ;;
            'systemctl daemon-reload') ;;
            'sysctl -p /etc/sysctl.d/60-agent-bus-redis-overcommit.conf') ;;
            'systemctl enable --now agent-bus-postgres-dump.timer') ;;
            *) fail "unexpected root command: $line" ;;
        esac
    done <"$RECORDER_LOG"
    if grep -Eq '(restart|stop|start|reload-or|kill) .*(redis|postgres)' "$RECORDER_LOG" "$SYSTEMCTL_LOG"; then
        fail "a backend service was started, stopped or restarted"
    fi
    printf 'installer-never-restarts-backend-services: PASS\n'
}
test_installer_never_restarts_backend_services

# Protects: a wrong AOF path is caught at install time. The preflight treats a missing
# manifest as a clean start, so a wrong --redis-aof-dir would otherwise protect nothing.
# Detects: installing against a manifest that does not exist without --allow-missing-aof.
# Needs: bash and the recorder, systemctl, sysctl and getent fakes.
# Breadcrumb: packaging/libexec/agent-bus-redis-aof-preflight
test_installer_missing_aof_manifest_refuses() {
    export RECORDER_TEST_RC=1
    run_hardening --only redis
    [[ $rc -eq 1 ]] || fail "missing manifest not refused (rc=$rc)"
    [[ "$out" == *"AOF manifest /var/lib/redis/appendonlydir/appendonly.aof.manifest not found"* ]] ||
        fail "refusal not explained: $out"
    if grep -q '^install' "$RECORDER_LOG"; then fail "files installed despite the refusal"; fi
    run_hardening --only redis --allow-missing-aof
    unset RECORDER_TEST_RC
    [[ $rc -eq 0 ]] || fail "--allow-missing-aof still refused: $out"
    printf 'installer-missing-aof-manifest-refuses: PASS\n'
}
test_installer_missing_aof_manifest_refuses

# Protects: the rendered files land with the right modes and content, and a second run
# changes nothing.
# Detects: wrong modes, a drop-in or unit that does not carry the configured settings, a
# dump directory that is not private, or a re-run that rewrites unchanged files.
# Needs: bash only; --root-dir writes under a fixture directory as the current user.
# Breadcrumb: packaging/systemd/agent-bus-postgres-dump.service.in
test_installer_root_dir_installs_idempotently() {
    local root="${test_root}/install-root"
    run_hardening --root-dir "$root" --pg-port 5432 --pg-databases "alpha beta" --redis-max-fix-bytes 2048
    [[ $rc -eq 0 ]] || fail "root-dir install failed: $out"
    [[ ! -s "$RECORDER_LOG" ]] || fail "root-dir install used sudo"
    [[ ! -s "$SYSTEMCTL_LOG" ]] || fail "root-dir install ran systemctl or sysctl"
    [[ "$(stat -c %a "${root}/usr/local/libexec/agent-bus/agent-bus-redis-aof-preflight")" == 755 ]] || fail "preflight mode"
    [[ "$(stat -c %a "${root}/usr/local/libexec/agent-bus/agent-bus-postgres-dump")" == 755 ]] || fail "dump mode"
    [[ "$(stat -c %a "${root}/etc/systemd/system/agent-bus-postgres-dump.service")" == 644 ]] || fail "unit mode"
    [[ "$(stat -c %a "${root}/var/backups/agent-bus-postgres")" == 700 ]] || fail "dump dir mode"
    cmp -s "$preflight" "${root}/usr/local/libexec/agent-bus/agent-bus-redis-aof-preflight" || fail "preflight copy differs"
    grep -qx 'ExecStartPre=/usr/local/libexec/agent-bus/agent-bus-redis-aof-preflight /var/lib/redis/appendonlydir appendonly.aof.manifest 2048' \
        "${root}/etc/systemd/system/redis-server.service.d/agent-bus-aof-preflight.conf" || fail "drop-in ExecStartPre"
    grep -qx 'ExecStart=/usr/local/libexec/agent-bus/agent-bus-postgres-dump /var/backups/agent-bus-postgres 5432 14 alpha beta' \
        "${root}/etc/systemd/system/agent-bus-postgres-dump.service" || fail "dump ExecStart"
    grep -qx 'ReadWritePaths=/var/backups/agent-bus-postgres' \
        "${root}/etc/systemd/system/agent-bus-postgres-dump.service" || fail "dump ReadWritePaths"
    grep -qx 'Persistent=true' "${root}/etc/systemd/system/agent-bus-postgres-dump.timer" || fail "timer Persistent"
    run_hardening --root-dir "$root" --pg-port 5432 --pg-databases "alpha beta" --redis-max-fix-bytes 2048
    [[ $rc -eq 0 ]] || fail "second run failed: $out"
    [[ "$(grep -c '^\[backend-hardening\] unchanged: ' <<<"$out")" -eq 6 ]] || fail "second run not all unchanged: $out"
    if grep -Eq '^\[backend-hardening\] (new|changed): ' <<<"$out"; then fail "second run rewrote a file"; fi
    printf 'installer-root-dir-installs-idempotently: PASS\n'
}
test_installer_root_dir_installs_idempotently

# Protects: invalid settings stop the installer before any file or command, including a
# Redis start limit that could never fire and a dump directory hidden by PrivateTmp
# (systemd status 226, measured 2026-10-26).
# Detects: each listed bad setting being accepted. The boundary burst x restart = interval
# is accepted in the same pass (positive control).
# Needs: bash only.
# Breadcrumb: packaging/systemd/redis-server.service.d/agent-bus-aof-preflight.conf.in
test_installer_refuses_invalid_settings() {
    local root="${test_root}/invalid-root" args
    local -a cases=(
        '--redis-start-limit-burst 61'
        '--redis-restart-sec 0'
        '--redis-overcommit-memory 3'
        '--redis-aof-dir relative/dir'
        '--redis-manifest appendonly.aof'
        '--pg-dump-dir /tmp/dumps'
        '--pg-dump-dir /var/tmp'
        '--pg-dump-dir /srv/with%specifier'
        '--pg-databases ../etc'
        '--pg-port 70000'
        '--pg-keep 0'
        '--only everything'
    )
    for args in "${cases[@]}"; do
        # shellcheck disable=SC2086 # word splitting builds the argument list on purpose
        run_hardening --root-dir "$root" $args
        [[ $rc -eq 2 ]] || fail "'$args' gave rc=$rc, want 2: $out"
        [[ ! -e "$root" ]] || fail "'$args' wrote files before refusing"
    done
    run_hardening --validate-only --redis-start-limit-burst 60
    [[ $rc -eq 0 ]] || fail "positive control: burst 60 x 5 s = 300 s refused: $out"
    [[ ! -s "$RECORDER_LOG" ]] || fail "invalid settings reached sudo"
    printf 'installer-refuses-invalid-settings: PASS\n'
}
test_installer_refuses_invalid_settings

# Protects: `install.sh --with-backend-hardening` reaches the hardening script with the
# operator's --backend-* overrides, and rejects bad ones before the build step.
# Detects: dropped or misparsed overrides, or a bad value discovered only after building.
# Needs: bash and an owned copy of install.sh with stub release binaries.
# Breadcrumb: install.sh
test_install_sh_forwards_backend_options() {
    local fixture="${test_root}/install-fixture" binary
    : >"$RECORDER_LOG"
    mkdir -p "${fixture}/target/release"
    cp "${repo_root}/install.sh" "${fixture}/install.sh"
    cp -R "${repo_root}/packaging" "${fixture}/packaging"
    for binary in agent-bus agent-bus-http agent-bus-mcp; do
        printf '#!/usr/bin/env bash\nexit 0\n' >"${fixture}/target/release/${binary}"
        chmod 755 "${fixture}/target/release/${binary}"
    done
    rc=0
    out="$(HOME="${test_root}/install-home" PATH="${fake_bin}:${PATH}" \
        AGENT_BUS_HARDENING_SUDO="${fake_bin}/sudo-recorder" bash "${fixture}/install.sh" \
        --skip-build --no-verify --dry-run --with-backend-hardening \
        --backend-only postgres --backend-pg-port=5432 --backend-pg-keep 3 2>&1)" || rc=$?
    [[ $rc -eq 0 ]] || fail "install.sh dry run failed: $out"
    [[ "$out" == *'ExecStart=/usr/local/libexec/agent-bus/agent-bus-postgres-dump /var/backups/agent-bus-postgres 5432 3 redis_backend workq'* ]] ||
        fail "overrides not forwarded: $out"
    [[ "$out" != *agent-bus-redis-aof-preflight* ]] || fail "--backend-only postgres not forwarded"
    rc=0
    out="$(HOME="${test_root}/install-home" bash "${fixture}/install.sh" --skip-build --no-verify \
        --with-backend-hardening --backend-pg-keep 0 2>&1)" || rc=$?
    [[ $rc -eq 2 ]] || fail "bad override gave rc=$rc, want 2"
    [[ "$out" != *"Deploying binaries"* ]] || fail "bad override found only after deploying"
    rc=0
    out="$(HOME="${test_root}/install-home" bash "${fixture}/install.sh" --skip-build --no-verify \
        --dry-run --backend-pg-port 5432 2>&1)" || rc=$?
    [[ $rc -eq 2 ]] || fail "--backend-* without --with-backend-hardening gave rc=$rc"
    [[ ! -s "$RECORDER_LOG" ]] || fail "install.sh dry run invoked sudo"
    printf 'install-sh-forwards-backend-options: PASS\n'
}
test_install_sh_forwards_backend_options

printf 'backend-hardening-test: all PASS\n'
