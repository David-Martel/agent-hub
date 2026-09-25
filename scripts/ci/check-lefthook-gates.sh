#!/usr/bin/env bash
# Fail if any lefthook command would silently run nothing.
#
# lefthook skips a command, and still exits 0, when its file filters match
# nothing ("no files for inspection" / "no matching staged files"). A
# `root: "."` on every command did exactly that in this repo: fmt, clippy,
# test and audit were skipped on every commit and push (lefthook 2.1.9),
# while each hook reported success.
#
# This drives the REAL hook path in a throwaway clone: it installs the hooks,
# overlays every command's `run` with an echo marker (globs, root and other
# filters are kept, since they are what is under test), modifies one tracked
# file matching each command's glob, then commits and pushes to a local bare
# remote. Every command must print its marker and none may report a skip.
#
# The config under test is the WORKING TREE's lefthook.yml.
set -euo pipefail

repo="$(git rev-parse --show-toplevel)"
command -v lefthook >/dev/null || { echo "ERROR: lefthook is not installed" >&2; exit 1; }
command -v python3 >/dev/null || { echo "ERROR: python3 is required" >&2; exit 1; }

work="$(mktemp -d "${RUNNER_TEMP:-/tmp}/lefthook-gates.XXXXXX")"
trap 'rm -rf -- "$work"' EXIT

# A snapshot of HEAD with fresh history, not a clone: CI checkouts are
# shallow, and pushing from a shallow clone is refused.
mkdir "$work/clone"
git -C "$repo" archive HEAD | tar -x -C "$work/clone"
git init -q --bare "$work/remote.git"
cd "$work/clone"
cp "$repo/lefthook.yml" lefthook.yml
git init -q
# Hermetic git identity/signing/hooks: nothing from the host config applies.
git config user.name "lefthook-gate-probe"
git config user.email "lefthook-gate-probe@invalid"
git config commit.gpgsign false
git config core.hooksPath .git/hooks
git add -A
git commit -q -m "chore: probe base"
git remote add probe "$work/remote.git"
git push -q probe HEAD:refs/heads/probe   # before hooks are installed

lefthook dump --format json > "$work/config.json"
python3 - "$work/config.json" "$work" <<'PY'
import fnmatch, json, subprocess, sys
cfg = json.load(open(sys.argv[1]))
work = sys.argv[2]
tracked = subprocess.check_output(["git", "ls-files"], text=True).split()
hooks = {"pre-commit", "pre-push", "commit-msg"}
overlay, expected, probes, errors = [], [], set(), []
for hook, body in cfg.items():
    if hook not in hooks or not isinstance(body, dict):
        continue
    cmds = body.get("commands") or {}
    if cmds:
        overlay.append(f"{hook}:\n  commands:")
    for name, c in cmds.items():
        marker = f"LEFTHOOK_RAN:{hook}:{name}"
        overlay.append(f"    {name}:\n      run: echo {marker}")
        expected.append(marker)
        globs = c.get("glob") or []
        globs = [globs] if isinstance(globs, str) else globs
        if globs:
            match = next((f for f in tracked if any(fnmatch.fnmatch(f, g) for g in globs)), None)
            if match is None:
                errors.append(f"{hook}:{name}: glob {globs} matches no tracked file")
            else:
                probes.add(match)
open(f"{work}/clone/lefthook-local.yml", "w").write("\n".join(overlay) + "\n")
open(f"{work}/expected.txt", "w").write("\n".join(expected) + "\n")
# Fall back to any tracked file so glob-less commands still see a change.
open(f"{work}/probes.txt", "w").write("\n".join(sorted(probes) or tracked[:1]) + "\n")
if errors:
    print("\n".join(f"ERROR: {e}" for e in errors), file=sys.stderr)
    sys.exit(1)
PY

# --force installs into the clone's local .git/hooks even when a global
# core.hooksPath is set (as on hosts running git-guard). It is an install
# target flag, not a hook bypass.
if ! lefthook install --force > "$work/install.log" 2>&1; then
  echo "ERROR: lefthook install failed"; cat "$work/install.log"; exit 1
fi
while IFS= read -r f; do printf '\n' >> "$f"; git add -- "$f"; done < "$work/probes.txt"

set +e
git commit -m "chore: lefthook gate probe" > "$work/hooks.log" 2>&1
commit_rc=$?
git push probe HEAD:refs/heads/probe >> "$work/hooks.log" 2>&1
push_rc=$?
set -e
sed 's/\x1b\[[0-9;]*m//g' "$work/hooks.log" > "$work/hooks.clean.log"

fail=0
if (( commit_rc != 0 || push_rc != 0 )); then
  echo "ERROR: probe commit/push failed (commit=$commit_rc push=$push_rc)"; fail=1
fi
if grep -E '\(skip\)' "$work/hooks.clean.log"; then
  echo "ERROR: a lefthook command was skipped although its files changed"; fail=1
fi
ran=0
while IFS= read -r marker; do
  if grep -qxF "$marker" "$work/hooks.clean.log"; then
    ran=$((ran + 1))
  else
    echo "ERROR: $marker never ran"; fail=1
  fi
done < "$work/expected.txt"
total="$(grep -c . "$work/expected.txt")"
echo "lefthook gates that ran on a matching change: $ran/$total"
if (( fail )); then
  echo "---- hook output ----"; cat "$work/hooks.clean.log"
  exit 1
fi
