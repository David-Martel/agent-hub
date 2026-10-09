# Quality and validation doctrine (agent-hub)

This file is the **repo-local** quality contract for `David-Martel/agent-hub`.
It complements:

- Account-wide **git-guard** (`C:\codedev\git-guard`, installed hooks under
  `~/.git-hooks` / `~/.local/share/git-guard/current`) — secret scan, NUL
  hygiene, language-gated ruff/ast-grep/shellcheck/json.
- Repo **lefthook.yml** — Rust `fmt`, `clippy -D warnings`, isolated unit tests
  via `scripts/test-agent-bus-isolated.sh`, blocking `cargo audit`.
- Patterns borrowed from **vigil_ai** quality culture: cold-start from live
  state, falsifiable gates, no silent skips when a gate cannot run.

## Enforcement layers

| Layer | When | Blocks? | What |
|---|---|---|---|
| git-guard pre-commit | every commit on this machine | secrets/NUL/JSON/ruff/shellcheck BLOCK; many others WARN | global + `.qa-gate.conf` |
| lefthook pre-commit | if lefthook installed | fmt/clippy | Rust only |
| lefthook pre-push | if lefthook installed | isolated tests + cargo-audit | no live 6380/5300/8400 |
| GitHub Actions CI | PR/push | fmt, clippy, unit, integration (disposable backends), audit | multi-arch |
| Human/agent doctrine | always | process | see validation matrix below |

Install hooks in a fresh clone:

```powershell
lefthook install
# git-guard is account-wide; confirm:
git config --global --get core.hooksPath
# optional repo overlay already present: .qa-gate.conf
```

## Anti-reward-hack validation matrix

A change is **not done** if only green unit counts move. Require receipts:

1. **No live bus as fixture.** Tests must refuse ports 6380/5300/8400 (see
   `scripts/test-agent-bus-isolated.sh` and CI isolation env).
2. **Routing/DNS claims** need `agent-bus health --encoding json` showing
   `hub_identity`, `backend.mode=remote`, and the **expected** `backend.url`
   (name or IP). Bare `ok:true` is insufficient.
3. **MCP claims** need initialize + tools/list + `bus_health` with the same
   identity as CLI.
4. **Message claims** need send + read with a nonce (prefer `--repo` /
   `--thread-id` / `--from-agent`; note `read --tag` may 400 until SHAs align).
5. **Service claims** need the correct supervisor scope (ASUS:
   `systemctl --user`; Windows: NSSM `AgentHub` on **18400** is island-only).
6. **Cloud claims** need separate cloud tokens (`AGENT_BUS_TOKENS`), never the
   on-site hub token; MCP may still be 501 until Worker deploy catches up.

## Function / I/O validity (Rust adaptation of vigil_ai spirit)

- Prefer typed ops in `agent-bus-core`; surfaces parse → call → render only.
- Fail closed on unknown MCP args (`additionalProperties: false`).
- HTTP errors must not be reported as success with null admin/status.
- New filters (topic/tag/repo) must either work on **all** surfaces or reject
  explicitly — silent ignore is a defect class.
- Config tiers: env beats file; never set `AGENT_BUS_SERVER_URL(S)` on clients
  that should use multi-candidate `config.json`.

## DNS / multi-homed clients (dtm-p1gen7, dtm-carbon-two)

Do **not** hard-code a single static LAN IP as the only route. Use:

1. Network-location-aware `server_urls` + `sites` (`docs/network-location-routing.md`)
2. Stable names (`agent-bus-hub.vigil.lan`, Headscale/VPN names) with IP fallback
3. SSH loopback forward for campus (`127.0.0.1:18480`)
4. Cloudflare `https://agentbus.dtmventures.com` as **cloud** role only until
   hub↔cloud sync + tokens are proven (agent-hub#79/#110)

See `docs/fleet-dns-and-cloud-status.md` for current fleet status.

## Consistency across David-Martel / dtmventures repos

| Concern | Standard |
|---|---|
| Secrets | git-guard secret_scan BLOCK; never commit tokens |
| Conventional commits | preferred; agent trailer when agent-authored |
| Python | ruff E,F block via git-guard; project ruff/mypy when configured |
| Rust | fmt+clippy+isolated tests; no live bus |
| Docs drift | use stable paths; retain observation dates and revisions in content |

Improvements welcome: extend isolated test refusals, add hub_identity
assertions to smoke scripts, keep cloud fixtures separate from on-site tokens.
