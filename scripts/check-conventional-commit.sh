#!/usr/bin/env bash
# Keep the advisory conventional-commit policy portable across hook runners.
set -u

if [ "$#" -ne 1 ]; then
  echo "ERROR: expected one commit-message file" >&2
  exit 2
fi

rc=0
grep -qE '^(feat|fix|docs|style|refactor|perf|test|build|ci|chore|revert)' "$1" || rc=$?
case "$rc" in
  0) ;;
  1) echo "WARNING: Consider conventional commit format" ;;
  *) echo "ERROR: conventional-commit check could not run (grep exit $rc)" >&2; exit "$rc" ;;
esac
