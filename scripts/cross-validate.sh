#!/usr/bin/env bash
# Cross-platform validation: confirm the agent-bus workspace compiles and its
# library unit tests pass on Linux glibc, using the system Docker daemon.
#
# Local-execution-first: runs against the local Docker daemon. If Docker is not
# installed/running, prints a clear message and exits 0 (no-op) unless --strict
# is passed.
#
# Usage:
#   scripts/cross-validate.sh
#   scripts/cross-validate.sh --musl
#   scripts/cross-validate.sh --strict   # fail if Docker absent
#   scripts/cross-validate.sh --dry-run
set -euo pipefail

TAG="agent-bus-linux-validate"
MUSL=0
STRICT=0
DRY_RUN=0

for arg in "$@"; do
    case "$arg" in
        --musl) MUSL=1 ;;
        --strict) STRICT=1 ;;
        --dry-run) DRY_RUN=1 ;;
        --tag=*) TAG="${arg#*=}" ;;
        *) echo "Unknown argument: $arg" >&2; exit 2 ;;
    esac
done

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
DOCKERFILE="$REPO_ROOT/docker/Dockerfile.linux"

if [ ! -f "$DOCKERFILE" ]; then
    echo "Dockerfile not found at $DOCKERFILE" >&2
    exit 1
fi

build_args=(build -f "$DOCKERFILE" -t "$TAG")
if [ "$MUSL" -eq 1 ]; then
    build_args+=(--target musl)
fi
build_args+=("$REPO_ROOT")

echo "==> docker ${build_args[*]}"
if [ "$DRY_RUN" -eq 1 ]; then
    echo "[DRY-RUN] Docker cross-validation command was not executed."
    exit 0
fi

if ! command -v docker >/dev/null 2>&1; then
    msg="Docker is not installed or not on PATH; skipping Linux cross-platform validation."
    if [ "$STRICT" -eq 1 ]; then echo "ERROR: $msg" >&2; exit 1; fi
    echo "WARNING: $msg" >&2
    exit 0
fi

if ! docker info >/dev/null 2>&1; then
    msg="Docker CLI found but the daemon is not reachable; skipping Linux cross-platform validation."
    if [ "$STRICT" -eq 1 ]; then echo "ERROR: $msg" >&2; exit 1; fi
    echo "WARNING: $msg" >&2
    exit 0
fi

docker "${build_args[@]}"

echo
echo "Linux cross-platform validation passed (build + workspace lib tests)."
