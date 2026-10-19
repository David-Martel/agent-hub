// Readback for .github/workflows/deploy-agentbus-cloud.yml: exits 0 only when Cloudflare reports
// that agentbus-cloud's live deployment was built from the given commit. Run with Node directly
// (type stripping): `node tools/deploy-readback.ts <sha>`. Reads CLOUDFLARE_API_TOKEN and
// CLOUDFLARE_ACCOUNT_ID from the environment and never prints the token.
import { checkDeployment } from "./deploy-readback-core.ts";

const WORKER = "agentbus-cloud";
const sha = process.argv[2] ?? "";
const token = process.env.CLOUDFLARE_API_TOKEN ?? "";
const account = process.env.CLOUDFLARE_ACCOUNT_ID ?? "";
if (!token || !account) {
  console.error("deploy-readback: CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID must be set");
  process.exit(2);
}

const base = `https://api.cloudflare.com/client/v4/accounts/${account}/workers/scripts/${WORKER}`;
async function get(path: string): Promise<unknown> {
  const res = await fetch(base + path, { headers: { Authorization: `Bearer ${token}` } });
  const body = (await res.json()) as { success?: boolean; errors?: unknown; result?: unknown };
  if (!res.ok || body.success !== true) {
    throw new Error(`GET ${path} failed: HTTP ${res.status} ${JSON.stringify(body.errors)}`);
  }
  return body.result;
}

const deployments = await get("/deployments");
const first = checkDeployment(deployments, sha);
let result = first;
if (!first.ok && first.versionId) {
  const version = (await get(`/versions/${first.versionId}`)) as {
    annotations?: Record<string, unknown>;
  };
  result = checkDeployment(deployments, sha, version.annotations?.["workers/message"]);
}
console.log(`deploy-readback: ${result.reason}`);
process.exit(result.ok ? 0 : 1);
