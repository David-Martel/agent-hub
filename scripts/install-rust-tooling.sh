#!/usr/bin/env bash
# install-rust-tooling.sh — idempotent installer for agent-bus Rust tooling
#
# Installs missing tools only. Already-installed binaries are skipped.
# Works on Linux and macOS (WSL included).
#
# See docs/rust-acceleration.md for usage details on each tool.
#
# Usage:
#   bash scripts/install-rust-tooling.sh
#   bash scripts/install-rust-tooling.sh --dry-run

set -euo pipefail

DRY_RUN=0
if [[ "${1:-}" == "--dry-run" ]]; then
    DRY_RUN=1
fi

# ── colours ───────────────────────────────────────────────────────────────────
CYAN='\033[0;36m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RESET='\033[0m'

# ── state ─────────────────────────────────────────────────────────────────────
declare -a SUMMARY_TOOL=()
declare -a SUMMARY_STATUS=()
declare -a SUMMARY_DESC=()
FAILURES=0

# ── helpers ───────────────────────────────────────────────────────────────────

log_summary() {
    local tool="$1" status="$2" desc="$3"
    SUMMARY_TOOL+=("$tool")
    SUMMARY_STATUS+=("$status")
    SUMMARY_DESC+=("$desc")
}

install_cargo_bin() {
    local bin="$1"
    local crate="$2"
    local desc="$3"
    shift 3
    local extra_args=("$@")

    if command -v "$bin" &>/dev/null; then
        log_summary "$bin" "ALREADY INSTALLED" "$desc"
        return
    fi

    if [[ $DRY_RUN -eq 1 ]]; then
        log_summary "$bin" "WOULD INSTALL" "$desc"
        return
    fi

    echo -e "${CYAN}Installing $crate...${RESET}"
    if cargo install --locked "$crate" "${extra_args[@]}"; then
        log_summary "$bin" "INSTALLED" "$desc"
    else
        log_summary "$bin" "FAILED" "$desc"
        (( FAILURES++ )) || true
    fi
}

install_rustup_component() {
    local component="$1"
    local desc="$2"
    local toolchain="${3:-stable}"

    if rustup component list --toolchain "$toolchain" 2>/dev/null | grep -q "^${component} (installed)"; then
        log_summary "$component" "ALREADY INSTALLED" "$desc"
        return
    fi

    if [[ $DRY_RUN -eq 1 ]]; then
        log_summary "$component" "WOULD INSTALL" "$desc"
        return
    fi

    echo -e "${CYAN}Adding rustup component $component ($toolchain)...${RESET}"
    if rustup component add "$component" --toolchain "$toolchain"; then
        log_summary "$component" "INSTALLED" "$desc"
    else
        log_summary "$component" "FAILED" "$desc"
        (( FAILURES++ )) || true
    fi
}

print_summary() {
    echo ""
    echo "─── Installation Summary ─────────────────────────────────────────────"
    printf "%-28s %-20s %s\n" "Tool" "Status" "Description"
    printf "%-28s %-20s %s\n" "────────────────────────────" "────────────────────" "───────────────────────────────────"
    for i in "${!SUMMARY_TOOL[@]}"; do
        printf "%-28s %-20s %s\n" "${SUMMARY_TOOL[$i]}" "${SUMMARY_STATUS[$i]}" "${SUMMARY_DESC[$i]}"
    done
    echo ""
}

# ── prerequisite check ────────────────────────────────────────────────────────

if ! command -v cargo &>/dev/null; then
    echo "ERROR: cargo not found. Install Rust via https://rustup.rs" >&2
    exit 1
fi

if ! command -v rustup &>/dev/null; then
    echo "ERROR: rustup not found. Install Rust via https://rustup.rs" >&2
    exit 1
fi

# ── rustup components ─────────────────────────────────────────────────────────

install_rustup_component \
    "llvm-tools-preview" \
    "Required by cargo-llvm-cov for source-based coverage"

# ── cargo-installable tools ───────────────────────────────────────────────────

install_cargo_bin \
    "cargo-nextest" \
    "cargo-nextest" \
    "Faster test runner (alias: cargo ab-nextest)"

install_cargo_bin \
    "cargo-llvm-cov" \
    "cargo-llvm-cov" \
    "LLVM coverage reports (alias: cargo ab-cov)"

install_cargo_bin \
    "cargo-audit" \
    "cargo-audit" \
    "Dependency vulnerability audit (alias: cargo ab-audit)"

install_cargo_bin \
    "cargo-machete" \
    "cargo-machete" \
    "Unused dependency finder (alias: cargo ab-machete)"

install_cargo_bin \
    "cargo-flamegraph" \
    "flamegraph" \
    "CPU flamegraph profiler (alias: cargo ab-flame)"

install_cargo_bin \
    "samply" \
    "samply" \
    "ETW/perf profiler → Firefox Profiler UI"

install_cargo_bin \
    "tokio-console" \
    "tokio-console" \
    "Live Tokio async task dashboard"

# ── summary ───────────────────────────────────────────────────────────────────

print_summary

if [[ $FAILURES -gt 0 ]]; then
    echo -e "${YELLOW}WARNING: $FAILURES tool(s) failed to install. Check output above.${RESET}"
    exit 1
else
    echo -e "${GREEN}All tools ready.${RESET}"
fi
