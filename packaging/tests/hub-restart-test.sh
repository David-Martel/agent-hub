#!/usr/bin/env bash
# Protects scripts/hub-restart.sh: the dry run changes nothing, --apply installs, restarts and reads
# back through the HTTP service, and every readback failure rolls the binaries back (exit 1).
# Detects: a readback that talks to Redis/PostgreSQL directly instead of the service being restarted,
# one that accepts a local backend, another hub's URL, a missing database or storage, a growing store
# gap or a lost probe message, an --apply that starts without a baseline gap, and a rollback that
# does not restore the saved binaries.
# Needs: bash, git, python3, sha256sum. Uses fake cargo, systemctl and agent-bus; no service, build
# or network is touched.
# Breadcrumb: scripts/hub-restart.sh (ab, verify_hub, the baseline guard).
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
test_root="$(mktemp -d)"
case "$test_root" in
    /tmp/* | /var/tmp/*) ;;
    *) printf 'unsafe temporary directory: %s\n' "$test_root" >&2; exit 1 ;;
esac
trap 'rm -rf -- "$test_root"' EXIT

fixture_repo="${test_root}/repo"
fake_bin="${test_root}/fake-bin"
bin_dir="${test_root}/installed"
state_dir="${test_root}/state"
env_file="${test_root}/hub.env"
control="${test_root}/control"
mkdir -p "${fixture_repo}/scripts" "$fake_bin" "$bin_dir" "$state_dir" "$control"
cp "${repo_root}/scripts/hub-restart.sh" "${fixture_repo}/scripts/hub-restart.sh"
printf 'AGENT_BUS_AUTH_TOKEN=fixture-token\n' > "$env_file"
git -C "$fixture_repo" init -q
tree="$(git -C "$fixture_repo" write-tree)"
commit="$(git -C "$fixture_repo" -c user.name=t -c user.email=t@example.invalid commit-tree "$tree" -m fixture)"
git -C "$fixture_repo" update-ref HEAD "$commit"

# One fake agent-bus: the same file is the "installed" and the "built" binary, with a VERSION.
write_agent_bus() {
    local version="$1" target="$2"
    cat > "$target" <<EOF
#!/usr/bin/env bash
# fake agent-bus ${version}
control="${control}"
if [[ "\${1:-}" == "--version" ]]; then echo "agent-bus ${version}"; exit 0; fi
cmd="\$1"; shift
url="\${AGENT_BUS_SERVER_URLS:-}"
phase=before; [[ -e "\$control/restarted" ]] && phase=after
get() { cat "\$control/\$1.\$phase" 2>/dev/null || cat "\$control/\$1" 2>/dev/null || echo "\$2"; }
case "\$cmd" in
    health)
        echo "token_present=\${AGENT_BUS_AUTH_TOKEN:+1} url=\$url" >> "\$control/health-calls"
        mode=remote; [[ -z "\$url" ]] && mode=local
        shown="\$(get url "\$url")"
        gap="\$(get gap 0)"
        pg='"pg_message_count": 100,'
        [[ "\$(get pg_count yes)" == no ]] && pg=''
        printf '{"ok": true, "hub_identity": "asuspro13", "backend": {"mode": "%s", "url": "%s"}, "database_ok": %s, "storage_ready": %s, "stream_length": %s, %s "pg_dropped_writes": 0, "pg_write_errors": 0}\n' \\
            "\$mode" "\$shown" "\$(get database_ok true)" "\$(get storage_ready true)" "\$((100 + gap))" "\$pg"
        ;;
    presence-list) echo "claude online" ;;
    send)
        body=""; while [[ \$# -gt 0 ]]; do [[ "\$1" == --body ]] && body="\$2"; shift; done
        if [[ "\$(get roundtrip ok)" == ok ]]; then echo "\$body" > "\$control/last-probe"; fi
        ;;
    read) cat "\$control/last-probe" 2>/dev/null || true ;;
esac
EOF
    chmod 755 "$target"
}
write_other() {
    printf '#!/usr/bin/env bash\necho "%s ${2}"\n' "$1" > "$bin_dir/$1"
    chmod 755 "$bin_dir/$1"
}
install_old() {
    write_agent_bus old "$bin_dir/agent-bus"
    write_other agent-bus-http old
    write_other agent-bus-mcp old
}

cat > "${fake_bin}/cargo" <<EOF
#!/usr/bin/env bash
mkdir -p "\${CARGO_TARGET_DIR}/release"
cp "${control}/new-agent-bus" "\${CARGO_TARGET_DIR}/release/agent-bus"
for b in agent-bus-http agent-bus-mcp; do
    printf '#!/usr/bin/env bash\necho "%s new"\n' "\$b" > "\${CARGO_TARGET_DIR}/release/\$b"
    chmod 755 "\${CARGO_TARGET_DIR}/release/\$b"
done
EOF
cat > "${fake_bin}/systemctl" <<EOF
#!/usr/bin/env bash
echo "\$*" >> "${control}/systemctl.log"
case "\$*" in
    *" restart "*) touch "${control}/restarted" ;;
    *" show "*) printf 'ActiveState=active\nSubState=running\nExecMainStartTimestamp=now\n' ;;
esac
EOF
chmod 755 "${fake_bin}/cargo" "${fake_bin}/systemctl"
write_agent_bus new "${control}/new-agent-bus"

failures=0
fail() { printf 'FAIL: %s\n' "$*" >&2; failures=$((failures + 1)); }

# run NAME EXIT_WANTED ARGS...: runs the script in a fresh fixture, leaves output in $out.
run() {
    local name="$1" want="$2"
    shift 2
    rm -rf "$control/restarted" "$control/systemctl.log" "$control/health-calls" "$control/last-probe"
    install_old
    local rc=0
    out="$(PATH="${fake_bin}:$PATH" HUB_RESTART_HEALTH_WAIT_S=2 HUB_RESTART_GAP_WAIT_S=2 \
        bash "${fixture_repo}/scripts/hub-restart.sh" --ref HEAD --bin-dir "$bin_dir" \
        --state-dir "$state_dir" --env-file "$env_file" --url http://hub.test:8400/ "$@" 2>&1)" || rc=$?
    [[ $rc -eq $want ]] || fail "$name: exit $rc, wanted $want. Output tail: $(tail -5 <<<"$out")"
}
restarts() { if [[ -f "$control/systemctl.log" ]]; then grep -c ' restart ' "$control/systemctl.log" || true; else echo 0; fi; }
installed_version() { "$bin_dir/agent-bus" --version; }
reset_control() { find "$control" -maxdepth 1 -type f ! -name new-agent-bus -delete; }

# 1. Dry run: nothing installed, nothing restarted.
reset_control
run "dry-run" 0
[[ "$(installed_version)" == "agent-bus old" ]] || fail "dry-run: the installed binary changed"
[[ "$(restarts)" == 0 ]] || fail "dry-run: the service was restarted"
grep -q 'would run' <<<"$out" || fail "dry-run: no plan printed"

# 2. Apply, healthy: installed, restarted once, read back through the pinned URL with the token.
reset_control
run "apply" 0 --apply
[[ "$(installed_version)" == "agent-bus new" ]] || fail "apply: the new binary was not installed"
[[ "$(restarts)" == 1 ]] || fail "apply: expected 1 restart, got $(restarts)"
grep -q 'round trip ok' <<<"$out" || fail "apply: no round trip"
grep -q 'url=http://hub.test:8400$' "$control/health-calls" || fail "apply: health was not pinned to --url"
grep -q 'token_present=1' "$control/health-calls" || fail "apply: the token was not passed"
grep -q 'fixture-token' <<<"$out" && fail "apply: the token was printed"

# 3. The readback failures that must roll back: each leaves the old binary and exit 1.
check_rolls_back() {
    local name="$1"
    shift
    reset_control
    local setup
    for setup in "$@"; do printf '%s' "${setup#*=}" > "$control/${setup%%=*}"; done
    run "$name" 1 --apply
    [[ "$(installed_version)" == "agent-bus old" ]] || fail "$name: the old binary was not restored"
    [[ "$(restarts)" == 2 ]] || fail "$name: expected the restart and a rollback restart, got $(restarts)"
}
check_rolls_back "database down" "database_ok.after=false"
check_rolls_back "storage not ready" "storage_ready.after=false"
check_rolls_back "answers from another hub" "url.after=http://other.test:8400"
check_rolls_back "store gap grows" "gap.before=0" "gap.after=5"
check_rolls_back "probe message lost" "roundtrip.after=lost"

# 3b. --hub-identity: a different hub answering rolls back; the right one passes.
reset_control
run "wrong hub identity" 1 --apply --hub-identity some-other-hub
[[ "$(installed_version)" == "agent-bus old" ]] || fail "hub identity: the old binary was not restored"
grep -q "hub_identity='asuspro13'" <<<"$out" || fail "hub identity: mismatch not reported: $(tail -3 <<<"$out")"
reset_control
run "right hub identity" 0 --apply --hub-identity asuspro13

# 4. A local (unpinned) backend must not satisfy the readback: simulate by dropping the pin.
reset_control
sed 's/AGENT_BUS_SERVER_URL="\$HUB_URL" AGENT_BUS_SERVER_URLS="\$HUB_URL" //' \
    "${repo_root}/scripts/hub-restart.sh" > "${fixture_repo}/scripts/hub-restart.sh"
run "unpinned readback" 1 --apply
[[ "$(installed_version)" == "agent-bus old" ]] || fail "unpinned: the old binary was not restored"
grep -q "backend.mode='local'" <<<"$out" || fail "unpinned: a local backend was not rejected: $(tail -3 <<<"$out")"
cp "${repo_root}/scripts/hub-restart.sh" "${fixture_repo}/scripts/hub-restart.sh"

# 5. No baseline gap (PostgreSQL unreadable): --apply refuses before installing anything.
reset_control
printf 'no' > "$control/pg_count"
run "no baseline" 1 --apply
[[ "$(installed_version)" == "agent-bus old" ]] || fail "no baseline: a binary was installed"
[[ "$(restarts)" == 0 ]] || fail "no baseline: the service was restarted"
grep -q 'refusing to restart without a baseline' <<<"$out" || fail "no baseline: wrong message: $(tail -3 <<<"$out")"

# 6. --rollback restores the saved binaries and verifies, even with no baseline gap.
reset_control
backup="${test_root}/backup"
mkdir -p "$backup"
write_agent_bus saved "$backup/agent-bus"
write_other agent-bus-http saved
cp "$bin_dir/agent-bus-http" "$backup/agent-bus-http"
write_other agent-bus-mcp saved
cp "$bin_dir/agent-bus-mcp" "$backup/agent-bus-mcp"
rm -f "$control/restarted"
printf 'no' > "$control/pg_count.before"
install_old
rc=0
out="$(PATH="${fake_bin}:$PATH" HUB_RESTART_HEALTH_WAIT_S=2 HUB_RESTART_GAP_WAIT_S=2 \
    bash "${fixture_repo}/scripts/hub-restart.sh" --rollback "$backup" --apply --bin-dir "$bin_dir" \
    --state-dir "$state_dir" --env-file "$env_file" --url http://hub.test:8400 2>&1)" || rc=$?
[[ $rc -eq 0 ]] || fail "rollback: exit $rc. Output tail: $(tail -5 <<<"$out")"
[[ "$(installed_version)" == "agent-bus saved" ]] || fail "rollback: the saved binary was not restored"

if [[ $failures -ne 0 ]]; then
    printf 'hub-restart-test: %d failure(s)\n' "$failures" >&2
    exit 1
fi
printf 'hub-restart-test: PASS\n'
