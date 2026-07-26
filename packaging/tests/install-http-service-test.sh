#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)"
test_root="$(mktemp -d)"
case "$test_root" in
    /tmp/* | /var/tmp/*) ;;
    *) printf 'unsafe temporary directory: %s\n' "$test_root" >&2; exit 1 ;;
esac
trap 'rm -rf -- "$test_root"' EXIT

fixture_repo="${test_root}/repo"
fixture_home="${test_root}/home"
fake_bin="${test_root}/fake-bin"
mkdir -p \
    "${fixture_repo}/packaging/systemd" \
    "${fixture_repo}/target/release" \
    "$fixture_home" \
    "$fake_bin"

cp "${repo_root}/install.sh" "${fixture_repo}/install.sh"
cp \
    "${repo_root}/packaging/systemd/agent-bus-http.service.in" \
    "${fixture_repo}/packaging/systemd/agent-bus-http.service.in"
cp \
    "${repo_root}/packaging/systemd/agent-bus-relay.service.in" \
    "${fixture_repo}/packaging/systemd/agent-bus-relay.service.in"
chmod 755 "${fixture_repo}/install.sh"

for binary in agent-bus agent-bus-http agent-bus-mcp; do
    printf '#!/usr/bin/env bash\nexit 0\n' > "${fixture_repo}/target/release/${binary}"
    chmod 755 "${fixture_repo}/target/release/${binary}"
done

cat > "${fake_bin}/systemctl" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${INSTALL_TEST_SYSTEMCTL_LOG:?}"
EOF
cat > "${fake_bin}/loginctl" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "show-user" ]]; then
    printf 'Linger=yes\n'
    exit 0
fi
printf '%s\n' "$*" >> "${INSTALL_TEST_LOGINCTL_LOG:?}"
EOF
chmod 755 "${fake_bin}/systemctl" "${fake_bin}/loginctl"

export HOME="$fixture_home"
export PATH="${fake_bin}:${PATH}"
export INSTALL_TEST_SYSTEMCTL_LOG="${test_root}/systemctl.log"
export INSTALL_TEST_LOGINCTL_LOG="${test_root}/loginctl.log"

"${fixture_repo}/install.sh" \
    --skip-build \
    --no-verify \
    --with-http-service

unit="${fixture_home}/.config/systemd/user/agent-bus-http.service"
test -f "$unit"
grep -Fq "ExecStart=\"${fixture_home}/.local/bin/agent-bus-http\" --port 8400" "$unit"
grep -Fq "Environment=\"AGENT_BUS_CONFIG=${fixture_home}/.config/agent-bus/config.json\"" "$unit"
grep -Fq "EnvironmentFile=-${fixture_home}/.config/agent-bus/hub.env" "$unit"
grep -Fq "AGENT_BUS_REDIS_URL=redis://127.0.0.1:6380/0" "$unit"
grep -Fq "AGENT_BUS_DATABASE_URL=postgresql://postgres@127.0.0.1:5300/redis_backend" "$unit"
if grep -Eq '^ExecStart=.*agent-bus serve' "$unit"; then
    printf 'unit uses the generic CLI instead of agent-bus-http\n' >&2
    exit 1
fi
if command -v systemd-analyze >/dev/null 2>&1; then
    verify_output="$(systemd-analyze --user verify "$unit" 2>&1)" || {
        printf '%s\n' "$verify_output" >&2
        exit 1
    }
    if [[ -n "$verify_output" ]]; then
        printf '%s\n' "$verify_output" >&2
        exit 1
    fi
fi

grep -Fxq -- "--user daemon-reload" "$INSTALL_TEST_SYSTEMCTL_LOG"
grep -Fxq -- "--user enable agent-bus-http.service" "$INSTALL_TEST_SYSTEMCTL_LOG"
if grep -Eq '(^| )(start|restart|--now)( |$)' "$INSTALL_TEST_SYSTEMCTL_LOG"; then
    printf 'installer unexpectedly started or restarted the service\n' >&2
    exit 1
fi

first_sha="$(sha256sum "$unit" | awk '{print $1}')"
"${fixture_repo}/install.sh" \
    --skip-build \
    --no-verify \
    --with-http-service
second_sha="$(sha256sum "$unit" | awk '{print $1}')"
test "$first_sha" = "$second_sha"

dry_home="${test_root}/dry-home"
mkdir -p "$dry_home"
HOME="$dry_home" "${fixture_repo}/install.sh" \
    --skip-build \
    --no-verify \
    --with-http-service \
    --dry-run
test ! -e "${dry_home}/.config/systemd/user/agent-bus-http.service"

relay_bin="${test_root}/relay.sh"
printf '#!/usr/bin/env bash\nexit 0\n' > "$relay_bin"
chmod 755 "$relay_bin"
combined_output="$(
    HOME="$dry_home" "${fixture_repo}/install.sh" \
        --skip-build \
        --no-verify \
        --with-http-service \
        --with-relay-service \
        --relay-bin "$relay_bin" \
        --hub-url http://127.0.0.1:8400 \
        --dry-run 2>&1
)"
grep -Fq "Would run: systemctl --user enable agent-bus-http.service" <<< "$combined_output"
grep -Fq "Would run: systemctl --user enable agent-bus-relay.service" <<< "$combined_output"

guard_home="${test_root}/guard-home"
mkdir -p "${guard_home}/.config/agent-bus"
printf '{}\n' > "${guard_home}/.config/agent-bus/config.json"
chmod 644 "${guard_home}/.config/agent-bus/config.json"
if HOME="$guard_home" "${fixture_repo}/install.sh" \
    --skip-build \
    --no-verify \
    --with-http-service > "${test_root}/guard.log" 2>&1
then
    printf 'installer accepted group/world-readable service config\n' >&2
    exit 1
fi
grep -Fq "must not be group/world accessible" "${test_root}/guard.log"
test ! -e "${guard_home}/.local/bin/agent-bus"

# A health verification can create config.json after the initial preflight.
# The last-moment permission check must still reject that unsafe file before
# rendering or enabling the service unit.
cat > "${fixture_repo}/target/release/agent-bus" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == "health" ]]; then
    mkdir -p "${HOME}/.config/agent-bus"
    printf '{}\n' > "${HOME}/.config/agent-bus/config.json"
    chmod 644 "${HOME}/.config/agent-bus/config.json"
fi
exit 0
EOF
chmod 755 "${fixture_repo}/target/release/agent-bus"
toctou_home="${test_root}/toctou-home"
mkdir -p "$toctou_home"
if HOME="$toctou_home" "${fixture_repo}/install.sh" \
    --skip-build \
    --with-http-service > "${test_root}/toctou.log" 2>&1
then
    printf 'installer enabled a unit after verification created an unsafe config\n' >&2
    exit 1
fi
grep -Fq "must not be group/world accessible" "${test_root}/toctou.log"
test ! -e "${toctou_home}/.config/systemd/user/agent-bus-http.service"

printf 'install-http-service-test: PASS\n'
