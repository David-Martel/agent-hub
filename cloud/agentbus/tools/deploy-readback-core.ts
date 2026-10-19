// Pure check behind tools/deploy-readback.ts: does the Worker's live deployment carry the
// commit the deploy job built? `wrangler deploy --message "agent-hub <sha>"` writes that
// message into the `workers/message` annotation of the deployment and of its version, so the
// readback compares the full 40-hex SHA against what Cloudflare reports, not against what the
// job asked for. Kept free of Node and Workers APIs so test/deploy-readback.test.ts can run it
// under the vitest Workers pool, and free of non-erasable TypeScript so Node runs it directly.

export interface ReadbackResult {
  ok: boolean;
  reason: string;
  versionId?: string;
}

interface Deployment {
  created_on?: unknown;
  annotations?: Record<string, unknown>;
  versions?: Array<{ version_id?: unknown; percentage?: unknown }>;
}

const FULL_SHA = /^[0-9a-f]{40}$/;

// `deployments` is the `result` of GET /accounts/{id}/workers/scripts/{name}/deployments.
// `versionMessage` is the `workers/message` annotation of the version that deployment serves,
// when the caller fetched it; either annotation naming the SHA is enough.
export function checkDeployment(
  deployments: unknown,
  sha: string,
  versionMessage?: unknown,
): ReadbackResult {
  if (!FULL_SHA.test(sha)) {
    return { ok: false, reason: `expected a full 40-hex commit SHA, got ${JSON.stringify(sha)}` };
  }
  const list = (deployments as { deployments?: unknown } | null)?.deployments;
  if (!Array.isArray(list) || list.length === 0) {
    return { ok: false, reason: "Cloudflare reported no deployments for the Worker" };
  }
  const latest = [...(list as Deployment[])].sort((a, b) =>
    String(b.created_on ?? "").localeCompare(String(a.created_on ?? "")),
  )[0]!;
  const serving = (latest.versions ?? []).filter((v) => Number(v.percentage) === 100);
  if (serving.length !== 1 || typeof serving[0]!.version_id !== "string") {
    return { ok: false, reason: "the latest deployment does not serve exactly one version at 100%" };
  }
  const versionId = serving[0]!.version_id as string;
  const messages = [latest.annotations?.["workers/message"], versionMessage].filter(
    (m): m is string => typeof m === "string",
  );
  if (messages.some((m) => m.includes(sha))) {
    return { ok: true, reason: `deployment serves version ${versionId} built from ${sha}`, versionId };
  }
  return {
    ok: false,
    reason: `latest deployment (version ${versionId}) does not name ${sha}; messages: ${JSON.stringify(messages)}`,
    versionId,
  };
}
