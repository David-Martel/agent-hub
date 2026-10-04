#!/usr/bin/env bash
# Run ordinary tests without inheriting live fleet stores, routes or credentials.
set -eu

fixture_config="$(mktemp "${TMPDIR:-/tmp}/agent-bus-hooks.XXXXXXXX.json")"
trap 'rm -f -- "$fixture_config"' EXIT
printf '{}' > "$fixture_config"
native_config="$fixture_config"
case "$(uname -s)" in
  MINGW*|MSYS*|CYGWIN*) native_config="$(cygpath -m "$fixture_config")" ;;
esac

env -u AGENT_BUS_SERVER_URL -u AGENT_BUS_SERVER_URLS -u AGENT_BUS_SERVER_CANDIDATES \
  -u AGENT_BUS_AUTH_TOKEN -u AGENT_BUS_TEST_REDIS_URL -u AGENT_BUS_TEST_DATABASE_URL \
  -u AGENT_BUS_TEST_SERVER_URL -u REGEN_FIXTURES \
  AGENT_BUS_CONFIG="$native_config" AGENT_BUS_STARTUP_ENABLED=false \
  AGENT_BUS_REDIS_URL=redis://127.0.0.1:1/0 \
  AGENT_BUS_DATABASE_URL=postgresql://postgres@127.0.0.1:1/none \
  AGENT_BUS_HUB_CACHE_TTL_SECONDS=0 \
  cargo test --workspace --quiet
