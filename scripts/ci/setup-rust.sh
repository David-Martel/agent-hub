#!/usr/bin/env bash
set -euo pipefail

# Keep Cargo outputs private to a runner. A shared CARGO_TARGET_DIR is not safe:
# concurrent rustc processes and target-specific artifacts can corrupt each other.
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-${RUNNER_TEMP:-$PWD/.tmp}/agent-hub-target}"
export CARGO_INCREMENTAL=0
# New version has a separate daemon endpoint and cache; leave old serving daemons alone.
export SCCACHE_DIR="${SCCACHE_DIR:-$HOME/.cache/sccache/agent-hub/native-0.18}"
export SCCACHE_SERVER_PORT="${SCCACHE_SERVER_PORT:-4230}"
export SCCACHE_REDIS_KEY_PREFIX="${SCCACHE_REDIS_KEY_PREFIX:-agent-hub:${RUNNER_ARCH:-unknown}:v2:}"
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
  echo "cargo not found; installing rustup (rust-toolchain.toml picks the compiler)"
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
    sh -s -- -y --default-toolchain none --profile minimal
  # shellcheck source=/dev/null
  source "$CARGO_HOME/env"
fi

if command -v rustup >/dev/null 2>&1; then
  # Install the toolchain pinned by rust-toolchain.toml explicitly rather than relying on
  # rustup's implicit auto-install, which rustup 1.28.0 turned off and 1.28.1 made
  # configurable. With no argument, `toolchain install` installs the active (pinned) one.
  rustup toolchain install --profile minimal --no-self-update
  rustup component add rustfmt clippy
fi

mkdir -p "$CARGO_TARGET_DIR" "$SCCACHE_DIR"

expected_sccache_version="0.18.0"
install_sccache() {
  local target_triple archive_name release_url temp_dir expected actual result
  case "${RUNNER_ARCH:-}" in
    X64)
      target_triple="x86_64-unknown-linux-musl"
      expected="45f1447fbe231e3037bde351ef70677dd212216c8d62ae7ca409fecc4d6acc89"
      ;;
    ARM64)
      target_triple="aarch64-unknown-linux-musl"
      expected="2b3284d5da3b46a47dc4229e75bb7b88ac4aa99c8d754fb7d2f84997e5a4354a"
      ;;
    *)
      echo "warning: no pinned sccache artifact for RUNNER_ARCH=${RUNNER_ARCH:-unset}" >&2
      return 1
      ;;
  esac
  archive_name="sccache-v${expected_sccache_version}-${target_triple}.tar.gz"
  release_url="https://github.com/mozilla/sccache/releases/download/v${expected_sccache_version}"
  temp_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/sccache-install.XXXXXX")"
  result=0
  curl --proto '=https' --tlsv1.2 --connect-timeout 10 --max-time 60 \
    -fsSLo "$temp_dir/$archive_name" "$release_url/$archive_name" &&
    actual="$(sha256sum "$temp_dir/$archive_name" | awk '{print $1}')" &&
    [[ "$actual" == "$expected" ]] &&
    tar -xzf "$temp_dir/$archive_name" --strip-components=1 -C "$temp_dir" &&
    install -m 0755 "$temp_dir/sccache" "$sccache_tools/sccache" ||
    result=$?
  rm -rf -- "$temp_dir"
  return "$result"
}

# Install only into this runner job's private tool directory. An upgrade must not
# replace a serving cache executable in a host-wide .cargo/bin or .local/bin.
sccache_tools="${RUNNER_TEMP:-$PWD/.tmp}/agent-hub-native-tools"
mkdir -p "$sccache_tools"
export PATH="$sccache_tools:$PATH"
installed_sccache_version="$(
  sccache --version 2>/dev/null | awk '{print $2}' || true
)"
if [[ "$installed_sccache_version" != "$expected_sccache_version" ]]; then
  if install_sccache; then
    echo "Installed pinned sccache $expected_sccache_version"
    hash -r
  else
    echo "error: unable to install pinned sccache $expected_sccache_version" >&2
    exit 1
  fi
fi

installed_sccache_version="$(sccache --version | awk '{print $2}')"
if [[ "$installed_sccache_version" != "$expected_sccache_version" ]]; then
  echo "error: sccache version differs from pinned $expected_sccache_version" >&2
  exit 1
fi
if command -v sccache >/dev/null 2>&1; then
  # Remote/object-store settings, when used, belong in the runner service
  # environment. Do not place credentials in the workflow or repository.
  # This repository uses a dedicated endpoint. Attach/start only: never stop a
  # serving daemon because another job or owner may still be using it.
  if python3 -B scripts/ci/native_cache_preflight.py \
      --sccache "$(command -v sccache)" --rustc "$(rustup which rustc)" \
      --directory "$CARGO_TARGET_DIR" --cache-directory "$SCCACHE_DIR" \
      --version "$expected_sccache_version"; then
    RUSTC_WRAPPER="$(command -v sccache)"
    export RUSTC_WRAPPER
    echo "sccache enabled ($RUSTC_WRAPPER; cache directory $SCCACHE_DIR)"
  else
    echo "error: pinned sccache is unhealthy; refusing uncached CI fallback" >&2
    exit 1
  fi
else
  echo "error: required pinned sccache is unavailable" >&2
  exit 1
fi

if [[ -n "${GITHUB_PATH:-}" ]]; then
  # Actions prepends collected entries in reverse order on the next step.
  # Publish the private qualified client last so inherited clients cannot win.
  printf '%s\n' "$HOME/.cargo/bin" >> "$GITHUB_PATH"
  printf '%s\n' "$HOME/.local/bin" "$sccache_tools" >> "$GITHUB_PATH"
fi
if [[ -n "${GITHUB_ENV:-}" ]]; then
  {
    printf 'CARGO_HOME=%s\n' "$CARGO_HOME"
    printf 'CARGO_INCREMENTAL=%s\n' "$CARGO_INCREMENTAL"
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
