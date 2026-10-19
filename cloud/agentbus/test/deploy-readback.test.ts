// Protects: the deploy job's claim that agentbus-cloud runs the merged commit
// (.github/workflows/deploy-agentbus-cloud.yml, tools/deploy-readback.ts).
// Detects: a readback that passes when Cloudflare serves an older version, a split rollout, a
// missing deployment, or a short/blank SHA that would match any message.
// Needs: nothing live; the fixtures copy the shape of GET .../workers/scripts/{name}/deployments
// observed on 2026-10-19. Limits: it cannot prove the live API keeps that shape; the deploy job's
// own readback run against Cloudflare is the live check.
import { describe, expect, it } from "vitest";
import { checkDeployment } from "../tools/deploy-readback-core";

const SHA = "6c058890709502e33a934fd8c3439e84d4054e16";
const OLD = "491230fa4d6467c7907329faff0a7cb02d127fa3";

function deployments(message: string, created = "2026-10-19T17:00:00Z", percentage = 100) {
  return {
    deployments: [
      {
        created_on: "2026-10-15T18:29:32Z",
        annotations: { "workers/message": `agent-hub ${OLD}` },
        versions: [{ version_id: "old-version", percentage: 100 }],
      },
      {
        created_on: created,
        annotations: { "workers/message": message },
        versions: [{ version_id: "new-version", percentage }],
      },
    ],
  };
}

describe("checkDeployment", () => {
  it("passes when the newest deployment names the commit (positive control)", () => {
    const r = checkDeployment(deployments(`agent-hub ${SHA}`), SHA);
    expect(r.ok).toBe(true);
    expect(r.versionId).toBe("new-version");
  });

  it("fails when the newest deployment names a different commit", () => {
    expect(checkDeployment(deployments(`agent-hub ${OLD}`), SHA).ok).toBe(false);
  });

  it("picks the newest deployment by created_on, not array order", () => {
    const d = deployments(`agent-hub ${SHA}`, "2026-10-01T00:00:00Z");
    expect(checkDeployment(d, SHA).ok).toBe(false);
  });

  it("accepts the version annotation when the deployment carries no message", () => {
    const d = deployments("");
    expect(checkDeployment(d, SHA).ok).toBe(false);
    expect(checkDeployment(d, SHA, `agent-hub ${SHA}`).ok).toBe(true);
  });

  it("fails a split rollout that does not serve one version at 100%", () => {
    expect(checkDeployment(deployments(`agent-hub ${SHA}`, undefined, 50), SHA).ok).toBe(false);
  });

  it("fails when there are no deployments or the payload is malformed", () => {
    expect(checkDeployment({ deployments: [] }, SHA).ok).toBe(false);
    expect(checkDeployment(null, SHA).ok).toBe(false);
  });

  it("refuses a short or empty SHA, which would match any message", () => {
    expect(checkDeployment(deployments(`agent-hub ${SHA}`), "").ok).toBe(false);
    expect(checkDeployment(deployments(`agent-hub ${SHA}`), SHA.slice(0, 7)).ok).toBe(false);
  });
});
