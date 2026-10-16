# Agent-hub resumption checkpoint

Owner: `codex-p1-agenthub-reconcile`; checkpoint requested by the owner before
the session usage limit. Recorded 2026-10-16 UTC. This is a work handoff, not a
deployment or merge-acceptance receipt.

## Preserved work and evidence

- Hosting research is in [the availability proposal](reliability-and-hosting-plan.md).
  No paid resources, DNS, credentials or infrastructure were changed.
- The [message-mesh contract](message-mesh.md) documents local-first routing,
  equal ASUS/P1 message capture, delayed bidirectional relay and claim safety.
- The [source proposal packet](proposals/message-mesh/README.md) preserves the
  assembled implementation as a patch with source hashes and QA tooling.
  **SOURCE_ONLY / NOT_COMPILED / NOT_TESTED / NOT_DEPLOYED.**
- The bounded Redis health source has prior C2 evidence: fourteen focused tests,
  strict workspace Clippy, MCP build and actual Linux MCP startup/health/exit
  passed. Linux health took 0.895 seconds with seventeen tools and natural
  child exit. These private receipts do not establish a published release.
- Hook commits `f05452db5760022766dba5f9ea24063ec0d175d3` and
  `c9f1aab6d7abe5716622a9e3cf18612d9e284783` are signed local commits.
  Normal pre-commit formatting and strict workspace Clippy passed. The second
  commit explicitly permits rustdoc execution in its private temporary mount.

Private evidence root:
`.Codex/state/fleet-reconciliation/followthrough/`. Preserve this ignored data;
it includes source freezes, manifests, honest failures and reusable owned caches.
Do not add private configurations, journals, credentials, binary caches or raw
fleet payloads to Git.

## Resume in this order

1. Re-inventory exact Git heads, dirty/ignored work, worktrees, stashes, open PRs,
   active agents and claims. Renew ownership before editing. Use the ASUS bus
   through the established strict SSH route; do not infer inactivity from an
   unreachable host. Carbon remains off by owner instruction.
2. Finish publication of the isolated hook route through normal pre-push,
   fresh security audit, independent high-risk review, current-head CI and the
   ASUS merge train. The earlier pre-push **failed** at doctests. A focused
   reproduction confirmed Docker's implicit `/tmp` noexec mount and execution
   errno13 despite successful standalone test summaries. The exec fix passed
   pre-commit; its full pre-push/doctest/audit rerun remains **NOT_RUN**.
3. Publish/review the owned health patch with current-head CI and compatible
   native shipping qualification. The Bookworm QA binary requires GLIBC2.36;
   it is not a vigil1 GLIBC2.35 deployment artifact. Preserve that distinction.
4. Before compiling or applying the mesh proposal, fix the blocking bridge
   review finding: consume canonical `event.notifications` when present,
   including an explicit empty array; use legacy reconstruction only when the
   array is absent. Preserve notification stream IDs in recipient SSE events.
   Add detecting cursor and empty-array regressions.
5. Review the complete assembled source, extend exact test expectations for
   HTTP runtime, credentials and Root integration tests, and freeze a fresh
   complete source manifest. Run isolated formatting, strict Clippy, actual
   selected unit/CLI/HTTP tests and disposable Redis/PostgreSQL integration.
   Verify partition/restart/response-loss/concurrency/three-node-loop behavior,
   retained journals and explicit driver/socket/process settlement. Never use
   live bus ports or inherit live bus/provider credentials.
6. Build compatible Windows/Linux artifacts using the admitted runner/toolchain
   cohort. Coordinate runtime custody before ASUS or P1 service changes.
   Qualify P1 locally, then update service identity, private peer credentials,
   watchdog, client routing and documentation coherently. Confirm actual
   bidirectional delivery and recovery before claiming equal deployed hubs.
7. Keep Cloudflare authenticated read-quota recovery, desktop MCP token
   provenance, off-site hosting, Headscale recovery and archive restore as
   separate remaining gates. Provisioning and new spending are not completed.

## Custody and runtime boundaries

The primary lane is `fix/bounded-redis-health`; the linked owned worktree is
`worktrees/container-hook-gates`, branch `ci/isolated-hook-qa`. Keep it until its
publication/review disposition and a fresh preservation check. Foreign branch
`ci/fleet-artifact-store` and both existing stashes remain preserved. The central
workspace registry sees the live checkout/worktree but still labels their owner
unknown and retains missing historical rows; reconcile coverage without
rewriting unrelated owners' records.

ASUS remains the sole global claim authority. P1 currently runs an older NSSM
HTTP child on loopback18400; it is not a qualified replacement. No service,
client config, watchdog or provider was changed in this checkpoint. P1's current
tailnet identity was observed as `dtm-p1gen7`, IPv4 `100.64.0.1`; recheck before
deployment. The P1gen9 migration lane reported a separately named logical
snapshot and native local stores; this is not fleet-history equivalence.

The old infinite blocking HTTP notification bridge remains a separate shutdown
problem: stopping/joining new mesh workers does not prove managed-process exit.
Historical adoption, destroyed Redis projection recovery, held-event repair,
retention retirement, group membership admission and credential rotation/restart
also remain explicit work. Never downgrade accepted mesh requests to an older
binary that lacks mesh replay deduplication.

Operational receipts and final local commit IDs are recorded in the private
message-mesh phase ledger and agent-bus checkpoint message. Re-read them before
starting another build or retiring a worktree; do not reuse an old PASS receipt
as a current-head test result.
