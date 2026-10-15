#!/usr/bin/env bash
set -euo pipefail

# Upgrade only this job's npm CLI; leave image/host Node and npm installations alone.
policy="$(dirname "${BASH_SOURCE[0]}")/toolchain-policy.json"
mapfile -t pins < <(node - "$policy" <<'JS'
const fs = require('node:fs');
const policy = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
if (!/^[0-9]+\.[0-9]+\.[0-9]+$/.test(policy.npm) ||
    !/^sha512-[A-Za-z0-9+/]+={0,2}$/.test(policy.npm_integrity)) process.exit(1);
for (const key of ['node', 'npm', 'npm_integrity']) console.log(policy[key]);
JS
)
test "${#pins[@]}" -eq 3
test "$(node --version)" = "v${pins[0]}"
tools="$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/agent-hub-npm-tools.XXXXXX")"
archive="$tools/npm.tar.gz"
curl --fail --location --proto '=https' --tlsv1.2 --connect-timeout 10 --max-time 60 \
  "https://registry.npmjs.org/npm/-/npm-${pins[1]}.tgz" --output "$archive"
actual="$(node - "$archive" <<'JS'
const fs = require('node:fs');
const crypto = require('node:crypto');
console.log('sha512-' + crypto.createHash('sha512').update(fs.readFileSync(process.argv[2])).digest('base64'));
JS
)"
test "$actual" = "${pins[2]}" || { echo 'Pinned npm archive integrity differs' >&2; exit 1; }
tar --no-same-owner -xzf "$archive" -C "$tools"
mkdir "$tools/bin"
cat > "$tools/bin/npm" <<'SH'
#!/usr/bin/env bash
exec node "$(dirname "$0")/../package/bin/npm-cli.js" "$@"
SH
chmod 0755 "$tools/bin/npm"
cat > "$tools/bin/npx" <<'SH'
#!/usr/bin/env bash
exec node "$(dirname "$0")/../package/bin/npx-cli.js" "$@"
SH
chmod 0755 "$tools/bin/npx"
touch "$tools/empty-userconfig"
export PATH="$tools/bin:$PATH"
export NPM_CONFIG_USERCONFIG="$tools/empty-userconfig"
export NPM_CONFIG_CACHE="$tools/cache"
test "$(npm --version)" = "${pins[1]}"
printf 'Qualified private npm=%s integrity=%s tools=%s\n' "${pins[1]}" "$actual" "$tools"
if [[ -n "${GITHUB_PATH:-}" ]]; then printf '%s\n' "$tools/bin" >> "$GITHUB_PATH"; fi
if [[ -n "${GITHUB_ENV:-}" ]]; then
  printf 'NPM_CONFIG_USERCONFIG=%s\nNPM_CONFIG_CACHE=%s\n' \
    "$NPM_CONFIG_USERCONFIG" "$NPM_CONFIG_CACHE" >> "$GITHUB_ENV"
fi
