#!/usr/bin/env bash
# install.sh — idempotent installer for the agent-hub Rust workspace (Linux/macOS/WSL).
#
# Builds the three release binaries (agent-bus, agent-bus-http, agent-bus-mcp),
# deploys them as REAL FILES to ~/.local/bin/, and (re)creates ~/.local/bin/*.exe
# symlinks pointing at target/release/ for Windows-parity tooling that expects a
# `.exe` suffix on this box.
#
# Safe to re-run: unchanged binaries (matched by sha256) are skipped, and any
# binary about to be overwritten is preserved first in a single timestamped
# backup directory (not scattered *.bak files).
#
# agent-bus-http and agent-bus-mcp are SERVER binaries — they block in serve
# mode and do not support `--version`. Version/health verification therefore
# only ever invokes `agent-bus --version` / `agent-bus health`, never the
# other two binaries.
#
# Usage:
#   ./install.sh [OPTIONS]
#
# Options:
#   --skip-build            Skip `cargo build --release --bins`; deploy whatever
#                            is already in target/release/ (fails if missing).
#   --no-verify-health       Skip the post-install `agent-bus health` check
#                            (agent-bus --version is always checked unless
#                            --no-verify is also given). Useful when the hub
#                            (asuspro13) is unreachable.
#   --no-verify              Skip ALL post-install verification (version + health).
#   --force                  Redeploy binaries even if sha256 already matches.
#   --bin-dir <path>         Override the deploy directory (default: ~/.local/bin).
#   --with-relay-service     Render packaging/systemd/agent-bus-relay.service.in
#                            with this machine's hostname and install it via
#                            `systemctl --user` (enabled, NOT started).
#   --relay-bin <path>       Path to the agent-bus-relay.sh wrapper script used
#                            by the relay unit (default: autodetect under
#                            ~/.config/agent-bus-relay/agent-bus-relay.sh).
#   --dry-run                Print planned actions; make no changes.
#   -h, --help                Show this help and exit.
#
# Exit codes: 0 success, non-zero on any failure (build, deploy, or verify).

set -euo pipefail

# ── constants / defaults ──────────────────────────────────────────────────────
SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" >/dev/null 2>&1 && pwd -P)"
REPO_ROOT="$SCRIPT_DIR"
BINARIES=(agent-bus agent-bus-http agent-bus-mcp)

BIN_DIR="${HOME}/.local/bin"
TARGET_DIR="${REPO_ROOT}/target/release"
SKIP_BUILD=0
NO_VERIFY_HEALTH=0
NO_VERIFY=0
FORCE=0
WITH_RELAY_SERVICE=0
RELAY_BIN=""
DRY_RUN=0

# ── colours (disabled when not a tty) ─────────────────────────────────────────
if [[ -t 1 ]]; then
    CYAN='\033[0;36m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; RESET='\033[0m'
else
    CYAN=''; GREEN=''; YELLOW=''; RED=''; RESET=''
fi

log()  { printf '%b[install]%b %s\n' "$CYAN" "$RESET" "$*"; }
ok()   { printf '%b[install]%b %s\n' "$GREEN" "$RESET" "$*"; }
warn() { printf '%b[install]%b %s\n' "$YELLOW" "$RESET" "$*" >&2; }
err()  { printf '%b[install]%b %s\n' "$RED" "$RESET" "$*" >&2; }

usage() {
    # Print the header comment block (everything between the shebang and the
    # first blank-then-`set -euo pipefail` line) as help text.
    sed -n '2,/^set -euo pipefail$/p' "${BASH_SOURCE[0]}" | sed '$d' | sed 's/^# \{0,1\}//'
}

# ── arg parsing ────────────────────────────────────────────────────────────────
while [[ $# -gt 0 ]]; do
    case "$1" in
        --skip-build) SKIP_BUILD=1; shift ;;
        --no-verify-health) NO_VERIFY_HEALTH=1; shift ;;
        --no-verify) NO_VERIFY=1; shift ;;
        --force) FORCE=1; shift ;;
        --bin-dir)
            [[ $# -ge 2 ]] || { err "--bin-dir requires a path argument"; exit 2; }
            BIN_DIR="$2"; shift 2 ;;
        --bin-dir=*) BIN_DIR="${1#--bin-dir=}"; shift ;;
        --with-relay-service) WITH_RELAY_SERVICE=1; shift ;;
        --relay-bin)
            [[ $# -ge 2 ]] || { err "--relay-bin requires a path argument"; exit 2; }
            RELAY_BIN="$2"; shift 2 ;;
        --relay-bin=*) RELAY_BIN="${1#--relay-bin=}"; shift ;;
        --dry-run) DRY_RUN=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) err "Unknown argument: $1"; usage; exit 2 ;;
    esac
done

# ── cargo resolution (handles a broken rustup shim) ───────────────────────────
# On this fleet, ~/.cargo/bin/cargo is a rustup shim. If rustup itself is
# missing/broken, `cargo` on PATH fails even though a real toolchain is
# installed under ~/.rustup/toolchains/<tc>/bin/. Fall back to the newest
# such toolchain, exporting its bin dir on PATH and RUSTUP_TOOLCHAIN so cargo
# invokes the matching rustc/clippy without depending on the shim.
resolve_cargo() {
    if command -v cargo >/dev/null 2>&1; then
        if cargo --version >/dev/null 2>&1; then
            log "Using cargo on PATH: $(command -v cargo)"
            return 0
        fi
        warn "cargo is on PATH but failed to run (broken rustup shim?); falling back to rustup toolchains."
    else
        warn "cargo not found on PATH; falling back to rustup toolchains."
    fi

    local rustup_dir="${RUSTUP_HOME:-$HOME/.rustup}/toolchains"
    if [[ ! -d "$rustup_dir" ]]; then
        err "No cargo on PATH and no rustup toolchains directory at $rustup_dir"
        return 1
    fi

    # Pick the toolchain dir with the newest cargo binary (mtime), preferring
    # a "stable-*" name on ties since that matches the fleet's default.
    local best="" best_mtime=-1
    for tc in "$rustup_dir"/*/; do
        local cargo_bin="${tc}bin/cargo"
        [[ -x "$cargo_bin" ]] || continue
        local mtime
        mtime="$(stat -c '%Y' "$cargo_bin" 2>/dev/null || echo -1)"
        if (( mtime > best_mtime )) || { (( mtime == best_mtime )) && [[ "$tc" == *"stable-"* ]]; }; then
            best="$tc"
            best_mtime="$mtime"
        fi
    done

    if [[ -z "$best" ]]; then
        err "No usable cargo binary found under $rustup_dir/*/bin/cargo"
        return 1
    fi

    local toolchain_name
    toolchain_name="$(basename "${best%/}")"
    export PATH="${best}bin:${PATH}"
    export RUSTUP_TOOLCHAIN="$toolchain_name"
    log "Resolved cargo via toolchain: $toolchain_name (${best}bin)"

    if ! cargo --version >/dev/null 2>&1; then
        err "Fallback cargo at ${best}bin/cargo still fails to run"
        return 1
    fi
    return 0
}

# ── build ──────────────────────────────────────────────────────────────────────
run_build() {
    if [[ $SKIP_BUILD -eq 1 ]]; then
        log "Skipping build (--skip-build); using existing $TARGET_DIR binaries."
        return 0
    fi

    if [[ $DRY_RUN -eq 1 ]]; then
        log "[dry-run] Would resolve cargo and run: cargo build --release --bins"
        return 0
    fi

    resolve_cargo || exit 1

    # sccache is configured globally as rustc-wrapper (~/.cargo/config.toml);
    # respect it by NOT overriding RUSTC_WRAPPER here.
    log "Building release binaries (cargo build --release --bins)..."
    ( cd "$REPO_ROOT" && cargo build --release --bins )
    ok "Build complete."
}

# ── verify binaries exist post-build ───────────────────────────────────────────
verify_built_binaries() {
    local missing=0
    for bin in "${BINARIES[@]}"; do
        if [[ ! -x "${TARGET_DIR}/${bin}" ]]; then
            err "Expected built binary missing: ${TARGET_DIR}/${bin}"
            missing=1
        fi
    done
    if [[ $missing -eq 1 ]]; then
        err "One or more release binaries are missing. Run without --skip-build, or build manually first."
        exit 1
    fi
}

# ── sha256 helper (portable: sha256sum on Linux, shasum -a 256 on macOS) ──────
sha256_of() {
    local f="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$f" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$f" | awk '{print $1}'
    else
        err "Neither sha256sum nor shasum is available; cannot verify idempotency."
        exit 1
    fi
}

# ── atomic deploy of real binaries + backup of anything overwritten ──────────
BACKUP_DIR=""
ensure_backup_dir() {
    if [[ -z "$BACKUP_DIR" ]]; then
        BACKUP_DIR="${BIN_DIR}/.agent-hub-installer-backups/$(date -u +%Y%m%dT%H%M%SZ)"
        if [[ $DRY_RUN -eq 0 ]]; then
            mkdir -p "$BACKUP_DIR"
        fi
        log "Backup directory for this run: $BACKUP_DIR"
    fi
}

deploy_binaries() {
    if [[ $DRY_RUN -eq 1 ]]; then
        log "Deploying binaries to $BIN_DIR (would create if missing)"
    else
        mkdir -p "$BIN_DIR"
        log "Deploying binaries to $BIN_DIR"
    fi

    for bin in "${BINARIES[@]}"; do
        local src="${TARGET_DIR}/${bin}"
        local dst="${BIN_DIR}/${bin}"

        if [[ ! -f "$src" ]]; then
            err "Built binary not found: $src"
            exit 1
        fi

        if [[ -f "$dst" && $FORCE -eq 0 ]]; then
            local src_sha dst_sha
            src_sha="$(sha256_of "$src")"
            dst_sha="$(sha256_of "$dst")"
            if [[ "$src_sha" == "$dst_sha" ]]; then
                log "  $bin: up-to-date (sha256 ${src_sha:0:12}...), skipping."
                continue
            fi
        fi

        if [[ $DRY_RUN -eq 1 ]]; then
            if [[ -f "$dst" ]]; then
                log "  [dry-run] Would back up existing $dst and replace with new build."
            else
                log "  [dry-run] Would install new $dst."
            fi
            continue
        fi

        if [[ -f "$dst" ]]; then
            ensure_backup_dir
            cp -p "$dst" "${BACKUP_DIR}/${bin}"
            log "  $bin: backed up prior binary to ${BACKUP_DIR}/${bin}"
        fi

        # Atomic deploy: copy to a temp file in the same directory, then
        # rename into place. `mv` within the same filesystem is an atomic
        # rename on Linux, so a concurrently-running process holding the old
        # inode open is unaffected and readers never see a partial file.
        local tmp
        tmp="$(mktemp "${dst}.XXXXXX")"
        cp "$src" "$tmp"
        chmod 755 "$tmp"
        mv -f "$tmp" "$dst"
        ok "  $bin: deployed -> $dst"
    done
}

# ── .exe symlinks for Windows-parity tooling ──────────────────────────────────
# Mirrors the existing on-disk convention on this box: ~/.local/bin/<bin>.exe is
# an ABSOLUTE symlink to target/release/<bin> (not a relative symlink, and not
# a copy of the deployed real file — it always tracks the freshest build).
create_exe_symlinks() {
    for bin in "${BINARIES[@]}"; do
        local target="${TARGET_DIR}/${bin}"
        local link="${BIN_DIR}/${bin}.exe"

        if [[ $DRY_RUN -eq 1 ]]; then
            log "  [dry-run] Would ensure symlink $link -> $target"
            continue
        fi

        if [[ -L "$link" ]] && [[ "$(readlink "$link")" == "$target" ]]; then
            log "  $bin.exe: symlink already correct, skipping."
            continue
        fi

        ln -sfn "$target" "$link"
        ok "  $bin.exe: symlink -> $target"
    done
}

# ── post-install verification ──────────────────────────────────────────────────
# IMPORTANT: only agent-bus supports --version. agent-bus-http and
# agent-bus-mcp are server binaries that block in serve mode on launch, so
# never invoke them here.
verify_install() {
    if [[ $NO_VERIFY -eq 1 ]]; then
        log "Skipping all post-install verification (--no-verify)."
        return 0
    fi

    if [[ $DRY_RUN -eq 1 ]]; then
        log "[dry-run] Would run: ${BIN_DIR}/agent-bus --version"
        [[ $NO_VERIFY_HEALTH -eq 1 ]] || log "[dry-run] Would run: ${BIN_DIR}/agent-bus health"
        return 0
    fi

    local cli="${BIN_DIR}/agent-bus"
    if [[ ! -x "$cli" ]]; then
        err "Cannot verify: $cli is not present/executable."
        exit 1
    fi

    log "Verifying: agent-bus --version"
    if ! "$cli" --version; then
        err "agent-bus --version failed."
        exit 1
    fi
    ok "Version check passed."

    if [[ $NO_VERIFY_HEALTH -eq 1 ]]; then
        log "Skipping health check (--no-verify-health)."
        return 0
    fi

    log "Verifying: agent-bus health"
    if ! "$cli" health; then
        err "agent-bus health failed. The hub may be unreachable from this client machine."
        err "Re-run with --no-verify-health to skip this check if the hub is known to be down."
        exit 1
    fi
    ok "Health check passed."
}

# ── optional systemd --user relay service ─────────────────────────────────────
install_relay_service() {
    if [[ $WITH_RELAY_SERVICE -eq 0 ]]; then
        return 0
    fi

    local template="${REPO_ROOT}/packaging/systemd/agent-bus-relay.service.in"
    if [[ ! -f "$template" ]]; then
        err "Relay service template not found: $template"
        exit 1
    fi

    local machine
    machine="$(hostname -s 2>/dev/null || hostname)"

    local relay_bin="$RELAY_BIN"
    if [[ -z "$relay_bin" ]]; then
        relay_bin="${HOME}/.config/agent-bus-relay/agent-bus-relay.sh"
    fi

    local config="${HOME}/.config/agent-bus/config.json"

    if [[ ! -x "$relay_bin" ]]; then
        warn "Relay wrapper script not found or not executable at: $relay_bin"
        warn "The relay unit will be installed but will fail to start until this script exists."
        warn "Pass --relay-bin <path> to point at the correct location."
    fi

    local unit_dir="${HOME}/.config/systemd/user"
    local unit_path="${unit_dir}/agent-bus-relay.service"

    if [[ $DRY_RUN -eq 1 ]]; then
        log "[dry-run] Would render $template -> $unit_path"
        log "[dry-run]   @MACHINE@ -> $machine"
        log "[dry-run]   @BIN@     -> $relay_bin"
        log "[dry-run]   @CONFIG@  -> $config"
        log "[dry-run] Would run: systemctl --user daemon-reload"
        log "[dry-run] Would run: systemctl --user enable agent-bus-relay.service"
        log "[dry-run] Would run: loginctl enable-linger \$USER (if not already enabled)"
        log "[dry-run] Would NOT start the service."
        return 0
    fi

    mkdir -p "$unit_dir"

    sed \
        -e "s|@MACHINE@|${machine}|g" \
        -e "s|@BIN@|${relay_bin}|g" \
        -e "s|@CONFIG@|${config}|g" \
        "$template" > "${unit_path}.tmp"
    mv -f "${unit_path}.tmp" "$unit_path"
    ok "Rendered relay unit -> $unit_path (machine=${machine})"

    if ! command -v systemctl >/dev/null 2>&1; then
        warn "systemctl not found; skipping daemon-reload/enable. Unit file was written but not installed."
        return 0
    fi

    systemctl --user daemon-reload
    systemctl --user enable agent-bus-relay.service
    ok "Enabled agent-bus-relay.service (not started — start manually with: systemctl --user start agent-bus-relay.service)"

    local linger_state
    linger_state="$(loginctl show-user "$(whoami)" --property=Linger 2>/dev/null | cut -d= -f2 || echo "")"
    if [[ "$linger_state" == "yes" ]]; then
        log "Linger already enabled for $(whoami); user services will run without an active login session."
    else
        log "Enabling linger for $(whoami) (systemctl --user services survive logout)..."
        if loginctl enable-linger "$(whoami)" 2>/dev/null; then
            ok "Linger enabled."
        else
            warn "Could not enable linger (may require polkit/root). Run manually: loginctl enable-linger $(whoami)"
        fi
    fi
}

# ── main ───────────────────────────────────────────────────────────────────────
main() {
    log "agent-hub installer starting (repo: $REPO_ROOT)"
    [[ $DRY_RUN -eq 1 ]] && warn "DRY RUN — no files, binaries, or services will be changed."

    run_build
    verify_built_binaries
    deploy_binaries
    create_exe_symlinks
    verify_install
    install_relay_service

    ok "Install complete."
}

main "$@"
