#!/usr/bin/env bash
set -euo pipefail

# Keep Cargo outputs private to a runner. A shared CARGO_TARGET_DIR is not safe:
# concurrent rustc processes and target-specific artifacts can corrupt each other.
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${RUNNER_TEMP:-$PWD/.tmp}/agent-hub-target}"
export SCCACHE_DIR="${SCCACHE_DIR:-$HOME/.cache/sccache/agent-hub}"
export SCCACHE_SERVER_PORT="${SCCACHE_SERVER_PORT:-4227}"
export SCCACHE_REDIS_KEY_PREFIX="${SCCACHE_REDIS_KEY_PREFIX:-agent-hub:${RUNNER_ARCH:-unknown}:v1:}"
export SCCACHE_REDIS_EXPIRATION="${SCCACHE_REDIS_EXPIRATION:-1209600}"

# The Spark pair exposes an unauthenticated Redis cache only on its private
# QSFP link. Prefer it for ARM64 fleet jobs, but never make cache availability
# a build prerequisite. Other targets remain local-cache-only.
redis_endpoint="${AGENT_HUB_SCCACHE_REDIS_ENDPOINT:-redis://10.55.152.2:6381}"
redis_address="${redis_endpoint#redis://}"
redis_host="${redis_address%%:*}"
redis_port="${redis_address##*:}"
if [[ "${RUNNER_ARCH:-}" == "ARM64" ]] &&
   timeout 2 bash -c "</dev/tcp/${redis_host}/${redis_port}" >/dev/null 2>&1; then
  export SCCACHE_MULTILEVEL_CHAIN="disk,redis"
  export SCCACHE_REDIS_ENDPOINT="$redis_endpoint"
  echo "Spark multilevel cache enabled (runner-local disk, then QSFP Redis)"
else
  unset SCCACHE_MULTILEVEL_CHAIN SCCACHE_REDIS_ENDPOINT
  echo "Remote cache unavailable or not applicable; using runner-local disk cache"
fi

if ! command -v cargo >/dev/null 2>&1; then
  if ! command -v curl >/dev/null 2>&1; then
    echo "cargo and curl are unavailable; cannot bootstrap Rust" >&2
    exit 127
  fi
  echo "cargo not found; installing the minimal stable rustup toolchain"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --default-toolchain stable --profile minimal
  # shellcheck source=/dev/null
  source "$CARGO_HOME/env"
fi

if command -v rustup >/dev/null 2>&1; then
  rustup component add rustfmt clippy
fi

mkdir -p "$CARGO_TARGET_DIR" "$SCCACHE_DIR"

expected_sccache_version="0.16.0"
install_sccache() {
  local target_triple archive_name release_url temp_dir expected actual result
  case "${RUNNER_ARCH:-}" in
    X64) target_triple="x86_64-unknown-linux-musl" ;;
    ARM64) target_triple="aarch64-unknown-linux-musl" ;;
    *)
      echo "warning: no pinned sccache artifact for RUNNER_ARCH=${RUNNER_ARCH:-unset}" >&2
      return 1
      ;;
  esac
  archive_name="sccache-v${expected_sccache_version}-${target_triple}.tar.gz"
  release_url="https://github.com/mozilla/sccache/releases/download/v${expected_sccache_version}"
  temp_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/sccache-install.XXXXXX")"
  result=0
  curl -fsSLo "$temp_dir/$archive_name" "$release_url/$archive_name" &&
    curl -fsSLo "$temp_dir/$archive_name.sha256" "$release_url/$archive_name.sha256" &&
    expected="$(tr -d '[:space:]' < "$temp_dir/$archive_name.sha256")" &&
    actual="$(sha256sum "$temp_dir/$archive_name" | awk '{print $1}')" &&
    [[ "$actual" == "$expected" ]] &&
    tar -xzf "$temp_dir/$archive_name" --strip-components=1 -C "$temp_dir" &&
    install -m 0755 "$temp_dir/sccache" "$HOME/.local/bin/sccache" ||
    result=$?
  rm -rf -- "$temp_dir"
  return "$result"
}

installed_sccache_version="$(
  sccache --version 2>/dev/null | awk '{print $2}' || true
)"
if [[ "$installed_sccache_version" != "$expected_sccache_version" ]]; then
  if install_sccache; then
    echo "Installed pinned sccache $expected_sccache_version"
    hash -r
  else
    echo "warning: unable to install pinned sccache $expected_sccache_version" >&2
  fi
fi

if command -v sccache >/dev/null 2>&1; then
  # Remote/object-store settings, when used, belong in the runner service
  # environment. Do not place credentials in the workflow or repository.
  # This repository uses a dedicated server port, so restarting cannot disrupt
  # caches used by other repositories on the same fleet host.
  sccache --stop-server >/dev/null 2>&1 || true
  if sccache --start-server >/dev/null 2>&1 && sccache --show-stats >/dev/null 2>&1; then
    RUSTC_WRAPPER="$(command -v sccache)"
    export RUSTC_WRAPPER
    echo "sccache enabled ($RUSTC_WRAPPER; cache directory $SCCACHE_DIR)"
  else
    unset RUSTC_WRAPPER
    echo "warning: sccache is unhealthy; continuing without a compiler wrapper" >&2
  fi
else
  unset RUSTC_WRAPPER
  echo "warning: sccache is not installed; continuing without a compiler wrapper" >&2
fi

if [[ -n "${GITHUB_PATH:-}" ]]; then
  printf '%s\n' "$HOME/.cargo/bin" >> "$GITHUB_PATH"
fi
if [[ -n "${GITHUB_ENV:-}" ]]; then
  {
    printf 'CARGO_HOME=%s\n' "$CARGO_HOME"
    printf 'CARGO_TARGET_DIR=%s\n' "$CARGO_TARGET_DIR"
    printf 'SCCACHE_DIR=%s\n' "$SCCACHE_DIR"
    printf 'SCCACHE_SERVER_PORT=%s\n' "$SCCACHE_SERVER_PORT"
    printf 'SCCACHE_REDIS_KEY_PREFIX=%s\n' "$SCCACHE_REDIS_KEY_PREFIX"
    printf 'SCCACHE_REDIS_EXPIRATION=%s\n' "$SCCACHE_REDIS_EXPIRATION"
    if [[ -n "${SCCACHE_MULTILEVEL_CHAIN:-}" ]]; then
      printf 'SCCACHE_MULTILEVEL_CHAIN=%s\n' "$SCCACHE_MULTILEVEL_CHAIN"
      printf 'SCCACHE_REDIS_ENDPOINT=%s\n' "$SCCACHE_REDIS_ENDPOINT"
    fi
    if [[ -n "${RUSTC_WRAPPER:-}" ]]; then
      printf 'RUSTC_WRAPPER=%s\n' "$RUSTC_WRAPPER"
    fi
  } >> "$GITHUB_ENV"
fi

cargo --version
rustc --version
