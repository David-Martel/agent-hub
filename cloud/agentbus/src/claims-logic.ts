/**
 * Pure claim/arbitration logic mirroring `agent-bus-core::channels`
 * (`claim_conflicts`, `recompute_claim_statuses`, `suggest_reroute`,
 * `REROUTE_RULES`, `effective_scope`, `is_machine_global_resource`). Kept
 * dependency-free and DO-storage-free so it can be unit tested directly.
 */

import type { OwnershipClaim, ResourceLeaseMode, ResourceScope, RerouteSuggestion } from "./types";
import { MAX_RESOURCE_NAME_LEN, ValidationError } from "./validation";

const MACHINE_GLOBAL_RESOURCES = [
  "~/bin",
  "bin-install",
  "cargo-target",
  "target-dir",
  "port:8400",
  "port:8401",
  "agent-bus-service",
  "sccache",
  "rustup",
];

export function isMachineGlobalResource(resource: string): boolean {
  const lower = resource.toLowerCase();
  return MACHINE_GLOBAL_RESOURCES.some((pattern) => lower.includes(pattern));
}

export function effectiveScope(resource: string, explicit?: ResourceScope): ResourceScope {
  if (explicit) return explicit;
  return isMachineGlobalResource(resource) ? "machine" : "repo";
}

/** Returns `true` if the two claims conflict (mirrors `claim_conflicts`). */
export function claimConflicts(left: OwnershipClaim, right: OwnershipClaim): boolean {
  const l = left.mode;
  const r = right.mode;
  if (l === "exclusive" || r === "exclusive") return true;
  if ((l === "shared" || l === "shared_namespaced") && r === "shared") return false;
  if (l === "shared" && r === "shared_namespaced") return false;
  if (l === "shared_namespaced" && r === "shared_namespaced") {
    const ln = left.namespace?.trim() || undefined;
    const rn = right.namespace?.trim() || undefined;
    if (ln !== undefined && rn !== undefined) return ln === rn;
    return true;
  }
  // Both "shared": no conflict (also covers the remaining shared/shared case
  // not already matched above).
  return false;
}

/** Mutates `claims` in place: `contested` if it conflicts with ANY other
 * claim in the set, else `granted`. Mirrors `recompute_claim_statuses`. */
export function recomputeClaimStatuses(claims: OwnershipClaim[]): void {
  for (let i = 0; i < claims.length; i++) {
    const current = claims[i]!;
    const conflicted = claims.some((other, j) => i !== j && claimConflicts(current, other));
    current.status = conflicted ? "contested" : "granted";
  }
}

const REROUTE_RULES: Array<[pattern: string, suggestedTmpl: string, hintTmpl: string]> = [
  ["cargo-target", "cargo-target:{agent}", "use --target-dir T:\\RustCache\\cargo-target-{agent}"],
  ["target-dir", "target-dir:{agent}", "use --target-dir T:\\RustCache\\cargo-target-{agent}"],
  ["coverage-output", "coverage-output:{agent}", "use --output-dir coverage-{agent}"],
  ["coverage", "coverage:{agent}", "use --output-dir coverage-{agent}"],
  ["bench-output", "bench-output:{agent}", "write benchmark results to bench-output-{agent}/"],
  ["bin-install", "bin-install:{agent}", "install to ~/.local/bin-{agent}"],
  ["~/bin", "bin-install:{agent}", "install to ~/.local/bin-{agent}"],
];

/** Mirrors `suggest_reroute`. */
export function suggestReroute(
  resource: string,
  agent: string,
  contestedBy: string[],
): RerouteSuggestion | undefined {
  if (!resource || !agent) return undefined;
  const others = contestedBy.length > 0 ? contestedBy.join(", ") : "another agent";
  const reason = `resource contested by ${others}`;
  const resourceLower = resource.toLowerCase();

  for (const [pattern, suggestedTmpl, hintTmpl] of REROUTE_RULES) {
    if (resourceLower.includes(pattern)) {
      return {
        original_resource: resource,
        suggested_resource: suggestedTmpl.replaceAll("{agent}", agent),
        isolation_hint: hintTmpl.replaceAll("{agent}", agent),
        reason,
      };
    }
  }

  return {
    original_resource: resource,
    suggested_resource: `${resource}:${agent}`,
    isolation_hint: `use namespaced resource '${resource}:${agent}' for isolation`,
    reason,
  };
}

export function parseLeaseMode(mode: string): ResourceLeaseMode {
  if (mode === "shared" || mode === "shared_namespaced" || mode === "exclusive") return mode;
  // Was a plain `Error` (500-mapped); a bad `mode` value is caller input, so
  // this should 400 like every other validation failure (agent-hub#82 review).
  throw new ValidationError(`invalid lease mode '${mode}'; expected shared|shared_namespaced|exclusive`);
}

export function parseResourceScope(scope: string): ResourceScope {
  if (scope === "repo" || scope === "machine") return scope;
  throw new ValidationError(`invalid scope '${scope}'; expected repo|machine`);
}

/**
 * Normalizes a claim resource name (agent-hub#82 review M9): folds
 * backslashes to forward slashes and lowercases, so `Foo.rs`/`foo.rs` and
 * `a\b`/`a/b` land in the same `ClaimDO` shard instead of each holding its
 * own independent "exclusive" grant. This is PARITY with the Rust hub's own
 * `channels.rs:372` backslash folding, plus an ADDITIONAL case-fold this
 * cloud tier needs that the Rust side does not yet have — see README.md's
 * "Resource aliasing" note: the two must be kept in sync, or a resource
 * claimed as `Foo.rs` on-site and `foo.rs` here will silently split across
 * two independent authorities once the Rust proxy (SYNC-CONTRACT.md §6)
 * exists. Also strips a leading `./` and caps length so an oversized or
 * relative-path resource name can't mint an unbounded number of ClaimDOs
 * (review M6).
 */
export function normalizeResourceName(resource: string): string {
  let normalized = resource.replace(/\\/g, "/").toLowerCase();
  if (normalized.startsWith("./")) normalized = normalized.slice(2);
  if (normalized.length === 0) {
    throw new ValidationError("resource must not be empty");
  }
  if (normalized.length > MAX_RESOURCE_NAME_LEN) {
    throw new ValidationError(`resource exceeds maximum length of ${MAX_RESOURCE_NAME_LEN}`);
  }
  return normalized;
}
