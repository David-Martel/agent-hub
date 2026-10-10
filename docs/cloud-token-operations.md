# Cloud token operations and routing

Original planning cutoff: 2026-10-08. Runtime state must be checked separately.

## Goals

- Generate and deploy `AGENT_BUS_TOKENS` for `https://agentbus.dtmventures.com`
  **without** putting secrets in shell environment variables or git.
- Keep on-site hub token and cloud tokens in **separate trust domains**.
- Survive multi-homed laptops (lab LAN, UMich, home) via name-based routes,
  SSH forward, Headscale/DERP, and cloud backup.
- Fail closed under stress: bad JSON shape, short tokens, on-site reuse,
  locked Bitwarden, missing backups.

## Toolkit

| Path | Role |
|---|---|
| `config/cloud-tokens.manifest.example.json` | Identity roster (no secrets) |
| `~/.config/agent-bus/cloud-tokens.manifest.json` | Local copy of roster (operator-edited) |
| `~/.config/agent-bus/cloud-tokens.json` | Full token map (ACL owner-only) |
| `~/.config/agent-bus/cloud-token` | Single client bearer for `token_file` |
| `~/.config/agent-bus/token-backups/` | Previous maps before rotate |
| `agent-bus cloud-tokens` | Canonical implementation for every action |
| `scripts/Manage-AgentBusCloudTokens.ps1` | Thin PowerShell forwarding wrapper |
| `scripts/manage_agentbus_cloud_tokens.py` | Thin Python forwarding wrapper |

Build the CLI with `cargo build -p agent-bus`; authenticated smoke requires
the default `server-mode` feature. From an installed binary, run
`agent-bus cloud-tokens --help`. The example roster includes ASUS, p1gen7,
Carbon and Work; update the local manifest's selected agent and host together.

### Production-map authority and source-only publication

Recovery equality proves that Bitwarden can recover the candidate local map.
It does **not** prove that the map contains every credential currently deployed
to the Worker. An identity roster or newly created recovery item cannot supply
that authority. When the authoritative deployed map is unknown, replacement,
mint/rotation for deployment, client activation and recovery-note replacement
remain **HOLD**. Preserve the opaque existing `AGENT_BUS_TOKENS` secret.

Source-only Worker publication can proceed independently when its separately
reviewed deployment preserves existing opaque secrets (strict keep-vars).
It must not invoke this toolkit's mint, recovery update, secret upload or
activation sequence as part of publication.

Before editing an existing recovery item, every recovered token and its exact
metadata must remain in the candidate map. Missing, malformed or empty existing
notes refuse the edit. A newly created recovery item is local recovery only;
it is never evidence of production completeness. Empty operational maps are
refused; revocation is a separate deliberate operation.

Upload has an enforced manifest gate, not merely an operator warning.
`deployed_map_authority` defaults to absent/UNKNOWN. Missing or invalid evidence
refuses both apply and upload preview before Bitwarden or Wrangler is invoked.
No force option can bypass this gate. Mint and recovery-upsert never stamp it.

The private local manifest may carry evidence supplied by the independently
verified owner of a currently authoritative stored baseline:

| Field | Required binding |
|---|---|
| `source_kind` | `independently-verified-stored-baseline` |
| `authority_reference` | Nonempty independent custody/provenance receipt reference |
| `baseline_path` | Existing valid, nonempty baseline, distinct from mutable toolkit map/client files |
| `baseline_sha256` | Full SHA256 of canonical baseline-map JSON |
| `candidate_sha256` | Full SHA256 of canonical candidate-map JSON |

Canonical JSON sorts token keys and all metadata object keys recursively,
preserving array order. Every exact baseline entry must remain in the candidate.
The validator checks these bindings; it cannot authenticate the receipt's
external origin or establish the baseline's current production authority.
Those facts must be independently established before supplying evidence.
Never use a minted map, copied candidate or newly created Bitwarden item to
invent that authority. Keep actual maps and proof metadata outside Git.
No authoritative production path or real proof is supplied by this toolkit.

### Operator sequence (p1gen7, only after deployed-map authority is established)

```powershell
cd C:\codedev\agent-bus

# 1) Materialize manifest (edit identities if needed)
pwsh -NoLogo -NoProfile -File .\scripts\Manage-AgentBusCloudTokens.ps1 -Action init-manifest

# 2) Start from the authoritative complete map; add identities preserving every entry
pwsh -NoLogo -NoProfile -File .\scripts\Manage-AgentBusCloudTokens.ps1 -Action mint

# 3) Write this host's client token_file
pwsh -NoLogo -NoProfile -File .\scripts\Manage-AgentBusCloudTokens.ps1 -Action write-client

# 4) Unlock Bitwarden through this host's supported tooling first
# Preserve the existing recovery baseline; verify candidate-map equality
pwsh -NoLogo -NoProfile -File .\scripts\Manage-AgentBusCloudTokens.ps1 -Action bw-upsert

# 5) Upload only with independent deployed-map authority AND recovery equality
pwsh -NoLogo -NoProfile -File .\scripts\Manage-AgentBusCloudTokens.ps1 -Action wrangler-put

# 6) Smoke public + authed
pwsh -NoLogo -NoProfile -File .\scripts\Manage-AgentBusCloudTokens.ps1 -Action smoke
```

Dry-run any step with `-DryRun`. Upload preview refuses unknown authority
before external tools. With valid independently supplied evidence it reads
Bitwarden for recovery equality but performs no secret upload.

`rotate` adds one credential for the selected client and writes a private
`cloud-token.pending` file. The current client file and all previous and
unrelated map entries remain in place. Complete `bw-upsert` and `wrangler-put`,
then call `activate`: it verifies exact recovery, authenticates the staged
credential against the cloud, and only then publishes the client file. Failed
verification retains the current client. Revocation is a separate operation.

### Anti-lockout

1. **Bitwarden is source of recovery** for the full map item name in the
   manifest (`agentbus.dtmventures.com AGENT_BUS_TOKENS`).
2. **Private local backups** under `token-backups/`, with GUID filenames,
   preserve the previous complete map before mint/rotate.
3. **On-site reuse refused** by fingerprint compare against
   `~/.config/agent-bus/config.json` `auth_token`.
4. **No User/Machine env token hydration** by this toolkit.
5. **Hard recovery gate:** upload requires an unlocked Bitwarden vault and
   exact JSON recovery equality of the local map. Independent deployed-map
   authority is still required. Missing/mismatched recovery
   stops before Wrangler starts. `-Force` cannot bypass this gate.
6. **Private creation:** Unix mode `0600` and a protected Windows user-only
   DACL are applied at exclusive file creation, before secret bytes are written.
   Failed publication removes only the private staging file it created.
7. **Bounded external tools:** Bitwarden and Wrangler have a 120-second
   original-process/pipe deadline and a shared two-second failure settlement.
   A timeout reports mutation outcome unknown; never blindly retry an upload.
8. **Smoke is required:** missing client files, rejected authentication,
   redirects, network failures and every unsuccessful HTTP status fail visibly.
9. Optional later: GitHub Actions secret `AGENT_BUS_CLOUD_TOKENS` mirrored
   from Bitwarden only via a human-approved `bws`/OIDC job; recovery still
   Bitwarden-first if GH is locked out.

## Cloudflare deploy notes

Worker name: `agentbus-cloud` (`cloud/agentbus/wrangler.toml`).

- Secrets via `wrangler secret put AGENT_BUS_TOKENS` (object JSON, ≥32-char keys).
- Do not enable `AGENT_BUS_DEV_ALLOW_SHARED_TOKEN` in production.
- Custom domain `agentbus.dtmventures.com`; `workers_dev=false` once live.
- Public `GET /health` is open; everything else needs bearer.

## Headscale / UMich routing (from headscale-ops)

UMich networks often block high-port/STUN **UDP**, leaving **DERP-over-443**
(TCP) as the reliable path. Design implications:

| Path | When | agent-bus candidate |
|---|---|---|
| Lab LAN name | `192.168.50.0/24` | `http://agent-bus-hub.vigil.lan:8400` |
| Fabric p2p | `10.60.0.0/16` | `http://asuspro13-p2p-<peer>:8400` |
| Campus / VPN | umich.edu suffix | `http://127.0.0.1:18480` SSH forward to hub |
| Tailnet / Headscale | `*.vpn.dtmventures.com` / MagicDNS | Prefer TCP/443 DERP; optional hub URL once path proven |
| World off-site | no on-site path | `https://agentbus.dtmventures.com` cloud role |

**Inbound world→UMich is not free:** do not assume exit-node “punch out”
semantics for unsolicited inbound to lab services. Prefer:

1. Client-initiated SSH reverse/forward from a UMich-reachable jump, or
2. Cloudflare edge (already public HTTPS) as the off-site coordination plane,
3. Headscale DERP for mesh **between enrolled nodes** when both sides can
   reach DERP on 443.

IronRDP/headscale-ops connection reliability notes (asuspro13 ↔ eng hosts)
emphasize control-plane health on **IPv4 TCP 443** and treating UDP as best
effort.

## Validation matrix

| Check | Pass criteria |
|---|---|
| mint | map file exists; roles hub/operator/agent present; ACL owner-only |
| fingerprint | no cloud token FP equals on-site hub token FP |
| public health | HTTP 200, `ok=true` |
| authed presence | HTTP 200 with client token after wrangler-put |
| unauth messages | HTTP 401 |
| LAN name health | `backend.url` uses `agent-bus-hub.vigil.lan` when on lab-lan |
| bus isolation | tests never target live 6380/5300/8400 |

## Related

- [Repository entrypoint](../README.md)
- [Example identity manifest](../config/cloud-tokens.manifest.example.json)
- agent-hub#79, #82, #110
- `docs/fleet-dns-and-cloud-status-2026-10-08.md`
- `docs/QUALITY_GATES.md`
- `cloud/agentbus/README.md`, `SYNC-CONTRACT.md`
