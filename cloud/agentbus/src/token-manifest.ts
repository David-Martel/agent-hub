/** Operator-only caller in index.ts. Hash the exact secret binding string,
 * never the authentication parser's filtered map or a reserialized object. */
export async function tokenManifest(binding: string | undefined) {
  if (typeof binding !== "string") throw new Error("token manifest unavailable");
  let parsed: unknown;
  try { parsed = JSON.parse(binding); } catch { throw new Error("token manifest unavailable"); }
  if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
    throw new Error("token manifest unavailable");
  }
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(binding));
  const sha256 = Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, "0")).join("");
  return { representation: "utf8-secret-binding-v1", sha256, entry_count: Object.keys(parsed).length };
}
