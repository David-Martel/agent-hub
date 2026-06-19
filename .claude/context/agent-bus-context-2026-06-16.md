# Agent-Bus Session Context — 2026-06-16

- **Context ID:** ctx-agent-bus-20260616
- **Created:** 2026-06-16T22:37:00Z (by: main / Claude Opus 4.8)
- **Project:** agent-bus (Rust, GitHub `David-Martel/agent-hub`)
- **Branch/Commit:** main @ `e3d2d95` (tag `v0.5.0`)
- **State:** clean, `main == origin/main`

## State Summary

Multi-session sweep covering PR triage, cross-machine ("vigil-fleet") connectivity,
versioning, and Windows tooling. Repo healthy: fmt/clippy(-D warnings)/764 unit
tests/all workspace doctests pass; 3 binaries build. CI + Release workflows were
re-enabled (were `disabled_manually` since 2026-03-25; still self-hosted-runner
dependent). First release tag `v0.5.0` cut + GitHub Release; `agent-bus --version`
now embeds git metadata (`0.5.0 (v0.5.0 2026-06-16)`) via a new `build.rs`.

The vigil-fleet (dtm-p1gen7 = this Windows laptop, asuspro13 = Linux hub,
spark-0060 + spark-3066 = DGX Spark aarch64 nodes) now has: full bidirectional
SSH mesh, a single shared agent-bus hub on **asuspro13:8400** that ALL nodes
(including dtm) point at, and versioned binaries deployed to dtm + both sparks.

## Recent Changes (merged PRs this sweep)

- #13 actions bumps; #14 cargo 16 deps incl. breaking redis 1.x/thiserror 2/reqwest 0.13 (+ my reqwest-0.13 test fix)
- #15 authed integration tests; #16 config audit; #17 batch-send server-mode (+ helper extract for clippy)
- #18 `scripts/cross-machine-health.ps1`; #19 `install-agent-hub-service.ps1 -AllowRemote`
- #20/#21 doctest fixes (crate-private examples → ignore; Settings::from_env)
- #22 `crates/agent-bus-cli/build.rs` git-version embedding + `cli.rs` VERSION const
- Tag `v0.5.0` + GitHub Release published

## Host / Fleet Changes (NOT in git — machine state)

- **dtm-p1gen7 AgentHub**: remote-enabled (`0.0.0.0:8400`, ALLOW_REMOTE, bearer auth via NSSM); Redis/PG stay loopback. Then **clients repointed to asus hub** (see decisions).
- **Firewall (dtm)**: rules "fleet SSH 22 (LAN+p2p)" and "agent-bus HTTP 8400 (LAN+p2p)" scoped to 192.168.50.0/24 + 10.60.0.0/16 + 10.55.152/153, profile Any.
- **SSH**: canonical fleet key `~/.ssh/id_ed25519` (= BW `ssh:David-Martel:fleet-ed25519`); enrolled on spark-3066; `~/.ssh/config` fleet Host entries added. Full 12-pair mesh works.
- **dtm clients repointed** to `http://asuspro13.local:8400`: config.json, .claude/mcp.json, .codex/config.toml, .gemini/settings.json, .claude.json (stdio→http). Shared fleet token (sha256 43c34a60…).
- **Binaries**: dtm `~/bin/*` real files + `~/.local/bin/*` symlinks→`~/bin`; both sparks `~/.local/bin` at v0.5.0. spark-3066 has repo clone at `~/src/agent-hub`.
- **CargoTools**: `~/bin/cargo-route.ps1` fixed (coerce Invoke-CargoRoute output to Int32 exit code).

## Decisions

- **dec-001 Hub topology**: asuspro13 = single fleet hub; dtm joined (clients repointed). dtm local bus = offline fallback. Rationale: true full-mesh agent comms; reverses prior dtm-as-primary.
- **dec-002 Bus exposure**: HTTP bus (8400) only exposed to LAN; Redis/PG stay loopback. Auth enforced (401 without token).
- **dec-003 Symlink scheme**: `~/.local/bin → ~/bin` (real deployed files), not → cargo cache. One deploy updates both.
- **dec-004 Versioning**: build.rs embeds `git describe`/sha/date; graceful `unknown` fallback; no runtime git dep.
- **dec-005 Cross-platform deploy**: build aarch64 on spark-3066 (only node w/ cargo; bypass broken sccache via `RUSTC_WRAPPER=""`), scp to spark-0060. asuspro13 (x86_64) deferred.
- **dec-006 Thunderbolt**: NOT used for bus (sparks are clients; QSFP is Spark↔Spark never-default; asus has no QSFP leg). TB/USB4 only optional bulk side-channel.

## Agent Work Registry

| Agent | Task | Status | Handoff |
|-------|------|--------|---------|
| general-purpose | Survey installed agent-bus tooling (~/.codex,.claude,.agents) | Complete | Found token/transport/stale-doc issues |
| general-purpose | Trace vigil-spark cross-machine topology | Complete | asuspro13=hub, dtm loopback-only (then fixed) |
| general-purpose | Validate build/test/clippy health | Complete | clean baseline; doctests not in CI (latent breakage) |
| general-purpose | Validate repos/tools/redis/pg/symlinks | Complete | no symlinks→fixed; split-brain→fixed |
| general-purpose | Research Thunderbolt/bridging topology | Complete | recommend LAN not TB; bind+firewall is real fix |
| main | PR triage/merge #13-22, SSH mesh, hub join, versioning, CargoTools fix | Complete | all verified |

## Roadmap / Open Items

**Immediate / deferred:**
- asuspro13 (x86_64) still on plain `0.5.0` — needs x86_64 build host for version embed (clone public repo + `RUSTC_WRAPPER="" cargo build`).
- spark-3066 sccache server broken ("Failed to read response header") — fix or keep bypassing.

**Tech debt / hygiene:**
- CI jobs are self-hosted-runner dependent (offline) → no real PR gating; consider moving core gates to ubuntu-latest.
- Add `cargo test --doc` to CI (doctests were a latent gap that broke build-deploy twice).
- `~/.claude.json` has case-different keys (PowerShell ConvertFrom-Json rejects; validate via python).
- 5 `worktree-agent-*` local branches in repo (pre-existing leftovers, not this session's).
- dtm→asuspro13 SSH now works; consider whether dtm should also have a management channel documented.
- main is unprotected but a repo ruleset requires review-thread resolution before merge (bot reviews must be resolved).

## Validation

- last_validated: 2026-06-16T22:37:00Z
- key commit: e3d2d95 (== origin/main), tag v0.5.0 pushed
- fmt/clippy/unit(764)/doctests: PASS as of this commit
- fleet: 12/12 SSH pairs OK; bidirectional bus messaging dtm↔sparks via asus OK
- is_stale: false
