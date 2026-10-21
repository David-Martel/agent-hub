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

# Protects installed artifact identity; detects aliases following a later build.
# Needs the owned Linux fixture only; breadcrumb: install.sh:create_exe_symlinks.
test_aliases_ignore_later_builds() {
    local binary installed before
    test ! -e "${fixture_home}/.local/bin/.agent-hub-installer-backups"
    for binary in agent-bus agent-bus-http agent-bus-mcp; do
        installed="${fixture_home}/.local/bin/${binary}"
        test "$(stat -c '%a' "$installed")" = 755
        if [[ "$(readlink "${installed}.exe")" != "$binary" ]]; then
            printf 'alias escaped installed artifact: %s\n' "$binary" >&2
            return 1
        fi
        before="$(sha256sum "$installed" | awk '{print $1}')"
        printf '# uninstalled rebuild\n' >> "${fixture_repo}/target/release/${binary}"
        test "$(sha256sum "${installed}.exe" | awk '{print $1}')" = "$before"
        cp "$installed" "${fixture_repo}/target/release/${binary}"
    done
    printf 'aliases-ignore-later-builds: PASS\n'
}
test_aliases_ignore_later_builds

# Protects destination security metadata; detects chmod755 or lost ACL/xattrs.
# Needs Linux stat; uv/libacl or ACL/xattr tools add controls or report SKIP.
# Breadcrumb: install.sh:deploy_binaries. Content timestamps/inodes may change.
test_upgrade_preserves_security_metadata() {
    local installed="${fixture_home}/.local/bin/agent-bus" metadata acl='' xattr='' extended=''
    local snapshot='import base64,json,os,sys; p=sys.argv[1]; print(json.dumps({n:base64.b64encode(os.getxattr(p,n)).decode() for n in os.listxattr(p)},sort_keys=True))'
    chmod 750 "$installed"
    if command -v uv >/dev/null && uv run --offline --no-project --no-python-downloads python -c 'import ctypes; ctypes.CDLL("libacl.so.1")' 2>/dev/null; then
        # Use the system ACL library and stdlib xattrs when command-line tools
        # are absent. No package installation or host configuration is needed.
        uv run --offline --no-project --no-python-downloads python -c '
import ctypes,os,sys
lib=ctypes.CDLL("libacl.so.1",use_errno=True)
lib.acl_from_text.argtypes=[ctypes.c_char_p]; lib.acl_from_text.restype=ctypes.c_void_p
lib.acl_set_file.argtypes=[ctypes.c_char_p,ctypes.c_int,ctypes.c_void_p]
lib.acl_free.argtypes=[ctypes.c_void_p]
acl=lib.acl_from_text(b"u::rwx,u:65534:r-x,g::r-x,m::r-x,o::---")
if not acl:
    raise RuntimeError("ACL fixture creation failed")
try:
    if lib.acl_set_file(os.fsencode(sys.argv[1]),0x8000,acl)!=0:
        raise OSError(ctypes.get_errno(), "ACL fixture assignment failed")
finally:
    lib.acl_free(acl)
os.setxattr(sys.argv[1],"user.agent_bus_install_test",b"retained")
' "$installed"
        extended="$(uv run --offline --no-project --no-python-downloads python -c "$snapshot" "$installed")"
    elif command -v setfacl >/dev/null && command -v getfacl >/dev/null; then
        setfacl -m u:65534:r-x "$installed"
        acl="$(getfacl -cp "$installed")"
    else
        printf 'upgrade ACL retention: SKIP (needs setfacl/getfacl)\n'
    fi
    if [[ -n "$extended" ]]; then
        : # Both real ACL and xattr controls were established above.
    elif command -v setfattr >/dev/null && command -v getfattr >/dev/null; then
        setfattr -n user.agent_bus_install_test -v retained "$installed"
        xattr="$(getfattr --only-values -n user.agent_bus_install_test "$installed" 2>/dev/null)"
    else
        printf 'upgrade xattr retention: SKIP (needs setfattr/getfattr)\n'
    fi
    metadata="$(stat -c '%a:%u:%g' "$installed")"
    printf '# reviewed replacement\n' >> "${fixture_repo}/target/release/agent-bus"
    "${fixture_repo}/install.sh" --skip-build --no-verify
    cmp "$installed" "${fixture_repo}/target/release/agent-bus"
    if [[ "$(stat -c '%a:%u:%g' "$installed")" != "$metadata" ]]; then
        printf 'upgrade changed destination mode/owner/group\n' >&2
        return 1
    fi
    [[ -z "$acl" ]] || test "$(getfacl -cp "$installed")" = "$acl"
    [[ -z "$xattr" ]] || test "$(getfattr --only-values -n user.agent_bus_install_test "$installed" 2>/dev/null)" = "$xattr"
    [[ -z "$extended" ]] || test "$(uv run --offline --no-project --no-python-downloads python -c "$snapshot" "$installed")" = "$extended"
    [[ -z "$extended" ]] || printf 'upgrade ACL+xattrs: PASS (libacl/os.xattr)\n'
    printf 'upgrade-preserves-security-metadata: PASS\n'
}
test_upgrade_preserves_security_metadata

# Protects the no-unsolicited-backup policy and existing operator data; detects
# automatic backup creation/deletion. Needs owned fixture; breadcrumb: deploy_binaries.
test_upgrade_creates_no_backups() {
    local backup_root="${fixture_home}/.local/bin/.agent-hub-installer-backups" before
    mkdir -p "$backup_root"
    printf 'operator-owned data\n' > "${backup_root}/keep"
    before="$(sha256sum "${backup_root}/keep" | awk '{print $1}')"
    printf '# update without backup\n' >> "${fixture_repo}/target/release/agent-bus"
    "${fixture_repo}/install.sh" --skip-build --no-verify
    test "$(sha256sum "${backup_root}/keep" | awk '{print $1}')" = "$before"
    test "$(find "$backup_root" -mindepth 1 -print | wc -l)" -eq 1
    printf 'upgrade-creates-no-backups: PASS\n'
}
test_upgrade_creates_no_backups

# Protects an existing installation on failed metadata cloning; detects a partial
# publication or leaked stage. Needs fake cp only; breadcrumb: deploy_binaries.
test_metadata_failure_preserves_installation() {
    local installed="${fixture_home}/.local/bin/agent-bus" before
    before="$(sha256sum "$installed" | awk '{print $1}')"
    export INSTALL_TEST_REAL_CP
    INSTALL_TEST_REAL_CP="$(command -v cp)"
    cat > "${fake_bin}/cp" <<'EOF'
#!/usr/bin/env bash
for argument in "$@"; do
    if [[ "$argument" == --attributes-only ]]; then exit 73; fi
done
exec "${INSTALL_TEST_REAL_CP:?}" "$@"
EOF
    chmod 755 "${fake_bin}/cp"
    printf '# metadata-copy failure candidate\n' >> "${fixture_repo}/target/release/agent-bus"
    if "${fixture_repo}/install.sh" --skip-build --no-verify > "${test_root}/metadata-failure.log" 2>&1; then
        printf 'installer accepted failed security metadata copy\n' >&2
        exit 1
    fi
    test "$(sha256sum "$installed" | awk '{print $1}')" = "$before"
    test -z "$(find "${fixture_home}/.local/bin" -maxdepth 1 -name 'agent-bus.??????' -print)"
    grep -Fq 'security metadata' "${test_root}/metadata-failure.log"
    rm -- "${fake_bin}/cp"
    cp "$installed" "${fixture_repo}/target/release/agent-bus"
    printf 'metadata-failure-preserves-installation: PASS\n'
}
test_metadata_failure_preserves_installation

# Protects hash-bound publication; detects mismatched staging or unreadable
# hashes treated as success. Needs fake hash tool; breadcrumb: deploy_binaries.
test_hash_failure_preserves_installation() {
    local installed="${fixture_home}/.local/bin/agent-bus" before mode
    before="$(sha256sum "$installed" | awk '{print $1}')"
    export INSTALL_TEST_REAL_SHA
    INSTALL_TEST_REAL_SHA="$(command -v sha256sum)"
    cat > "${fake_bin}/sha256sum" <<'EOF'
#!/usr/bin/env bash
if [[ "$1" == */agent-bus.?????? ]]; then
    if [[ "${INSTALL_TEST_HASH_MODE:?}" == error ]]; then exit 75; fi
    printf 'incorrect-digest  %s\n' "$1"
    exit 0
fi
exec "${INSTALL_TEST_REAL_SHA:?}" "$@"
EOF
    chmod 755 "${fake_bin}/sha256sum"
    printf '# rejected hash candidate\n' >> "${fixture_repo}/target/release/agent-bus"
    for mode in mismatch error; do
        if INSTALL_TEST_HASH_MODE="$mode" "${fixture_repo}/install.sh" --skip-build --no-verify > "${test_root}/hash-failure.log" 2>&1; then
            printf 'installer accepted failed staged hash: %s\n' "$mode" >&2
            exit 1
        fi
        grep -Fq 'Staged executable does not match source' "${test_root}/hash-failure.log"
        test "$(sha256sum "$installed" | awk '{print $1}')" = "$before"
        test -z "$(find "${fixture_home}/.local/bin" -maxdepth 1 -name 'agent-bus.??????' -print)"
    done
    rm -- "${fake_bin}/sha256sum"
    cp "$installed" "${fixture_repo}/target/release/agent-bus"
    printf 'hash-failure-preserves-installation: PASS\n'
}
test_hash_failure_preserves_installation

# Protects Darwin's silent cp-p ownership-loss boundary; detects publication on
# stat failure/mismatch. Needs simulated uname/stat only, NOT macOS qualification.
# Breadcrumb: install.sh:deploy_binaries Darwin branch.
test_simulated_darwin_stat_failure_preserves_installation() {
    local installed="${fixture_home}/.local/bin/agent-bus" before mode
    before="$(sha256sum "$installed" | awk '{print $1}')"
    export INSTALL_TEST_REAL_STAT
    INSTALL_TEST_REAL_STAT="$(command -v stat)"
    cat > "${fake_bin}/uname" <<'EOF'
#!/usr/bin/env bash
printf 'Darwin\n'
EOF
    cat > "${fake_bin}/stat" <<'EOF'
#!/usr/bin/env bash
if [[ "$1" == -f ]]; then
    if [[ "${INSTALL_TEST_STAT_MODE:?}" == error ]]; then exit 76; fi
    if [[ "$3" == */agent-bus.?????? ]]; then
        printf '0:0:0\n'
    else
        "${INSTALL_TEST_REAL_STAT:?}" -c '%a:%u:%g' "$3"
    fi
    exit 0
fi
exec "${INSTALL_TEST_REAL_STAT:?}" "$@"
EOF
    chmod 755 "${fake_bin}/uname" "${fake_bin}/stat"
    printf '# rejected simulated Darwin candidate\n' >> "${fixture_repo}/target/release/agent-bus"
    for mode in mismatch error; do
        if INSTALL_TEST_STAT_MODE="$mode" "${fixture_repo}/install.sh" --skip-build --no-verify > "${test_root}/darwin-failure.log" 2>&1; then
            printf 'installer accepted simulated Darwin stat failure: %s\n' "$mode" >&2
            exit 1
        fi
        grep -Fq 'security metadata' "${test_root}/darwin-failure.log"
        test "$(sha256sum "$installed" | awk '{print $1}')" = "$before"
        test -z "$(find "${fixture_home}/.local/bin" -maxdepth 1 -name 'agent-bus.??????' -print)"
    done
    rm -- "${fake_bin}/uname" "${fake_bin}/stat"
    cp "$installed" "${fixture_repo}/target/release/agent-bus"
    printf 'simulated-darwin-stat-failure-preserves-installation: PASS (not macOS qualification)\n'
}
test_simulated_darwin_stat_failure_preserves_installation

# Protects Windows security metadata from POSIX-only assumptions; detects
# accidental Git Bash deployment. Needs fake uname; breadcrumb: install.sh:main.
test_unsupported_platform_refuses_before_deployment() {
    cat > "${fake_bin}/uname" <<'EOF'
#!/usr/bin/env bash
printf 'MINGW64_NT\n'
EOF
    chmod 755 "${fake_bin}/uname"
    if HOME="${test_root}/unsupported-home" "${fixture_repo}/install.sh" --skip-build --no-verify > "${test_root}/unsupported.log" 2>&1; then
        printf 'installer accepted unsupported security metadata platform\n' >&2
        exit 1
    fi
    grep -Fq 'Unsupported installer platform' "${test_root}/unsupported.log"
    test ! -e "${test_root}/unsupported-home"
    rm -- "${fake_bin}/uname"
    printf 'unsupported-platform-refuses-before-deployment: PASS\n'
}
test_unsupported_platform_refuses_before_deployment

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
