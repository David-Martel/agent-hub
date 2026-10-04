# Candidate authentication and hub selection

Clients select an ordered hub list from one complete configuration tier:
`AGENT_BUS_SERVER_CANDIDATES` (JSON), `AGENT_BUS_SERVER_URLS` (comma-separated),
`AGENT_BUS_SERVER_URL`, config `server_urls`, then config `server_url`.
Empty legacy URL environment variables fall through. A malformed JSON tier
or unreadable/invalid configuration fails closed before backend access.

```json
{
  "server_urls": [
    "http://hub.internal:8400",
    {"url":"https://cloud.example.com", "role":"cloud", "token_file":"~/.config/agentbus-cloud/agent.token"}
  ],
  "probe_connect_timeout_ms": 750,
  "hub_cache_ttl_seconds": 60,
  "startup_enabled": false
}
```

The example uses placeholders. Keep secrets outside repositories and MCP
configuration. Token files must be readable only by the intended account.
`token_env` names an environment variable; `token_file` names a file read at
request time. They are mutually exclusive. Unknown keys, invalid URLs,
ambiguous candidates and multiple authorities are rejected.

Legacy string entries use the global on-site token only on trusted local or
private endpoints. Public endpoints and `cloud` candidates never receive that
token. An absent, unreadable or invalid explicit credential skips its
candidate; it never falls back to the global token. Requests use parsed
origin and path boundaries, and automatic redirects are disabled.

The first candidate defaults to `authoritative`; later candidates default to
`fallback`. Explicit roles override those defaults. Only one candidate may be
authoritative, and cloud candidates cannot grant authoritative claims.
Claim requests probe the authority directly even if an earlier read selected
a cached fallback. A configured but unreachable hub list reports offline;
it never silently creates a local fleet store.

The cross-process cache stores only a URL, role, candidate-source fingerprint
and timestamp. Its default TTL is 60 seconds; zero disables it. The cached
candidate is authenticated and probed again before reuse. Corrupt, expired,
future-dated or mismatched entries are misses. Writes are atomic and cache
failure is non-fatal. `AGENT_BUS_HUB_CACHE_FILE` overrides the platform cache
path. Long-lived MCP sessions also retain the existing brief process cache;
failed operations invalidate both caches.

## Current deployment boundaries

On DTM-P1GEN7 the authoritative fleet hub is ASUS, reached through the
configured fabric/LAN candidates. The Windows local NSSM hub on port 18400 is
an independent maintenance service and must not be inserted as a fallback
for fleet coordination. Session hooks must respect the client configuration
instead of forcing `localhost:8400`.

Cloudflare public reachability and authentication are distinct from route
support and synchronization. The live public health endpoint responded and
unauthenticated MCP required a bearer token during the October 4 audit.
The Worker source still defers authenticated MCP with HTTP 501. Its
standalone role-authenticated routes do not prove on-site synchronization,
claims replication, outbox delivery, historical import or deployment parity.
Do not activate the cloud candidate for the fleet until those requirements
and separate cloud credentials have their own validation receipts.

The Cloudflare token-map secret is a whole-map replacement. Coordinate with
its owner before modifying it; preserve existing hub/operator entries and
use distinct per-host agent credentials. Source hardening is not evidence
that retired credentials have been revoked.

## Validation and rollout

Run workspace formatting, strict Clippy, unit tests, real loopback transport
fixtures and the hosted disposable-backend integration jobs. Tests must use
a private configuration and closed backend ports, unset all three candidate
environment overrides and disable the shared resolution cache. Never use
the live Redis/PostgreSQL bus as a test fixture.

Review the exact PR head and required checks before merge. Install binaries
from the validated commit with a rollback copy and hash readback. Verify
each host and each fresh CLI/MCP process independently; replacing a file
does not update already-running agent processes.
