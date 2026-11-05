/**
 * Message-body excerpting for `GET /messages?excerpt=N`.
 *
 * Why this is not `body.slice(0, n)`: `String.length` and `slice` count UTF-16
 * code units, so a cut that lands between the two halves of a non-BMP
 * character (an emoji, say) leaves a lone surrogate. `JSON.stringify` writes
 * that as `\ud83d`, which the Rust client's `serde_json` refuses, and one such
 * message then fails the client's whole `read` (agent-hub#231). The native hub
 * counts Unicode scalar values (`output::excerpt_body`); this does the same, so
 * a given `excerpt=N` keeps the same characters whichever tier answers.
 */

/** Keep at most `max` Unicode scalar values of `body`, ending with `…` when cut. */
export function excerptBody(body: string, max: number): string {
  // Code units are never fewer than code points, so a body that fits in `max`
  // units fits in `max` points. This skips the allocation on the common path.
  if (body.length <= max) return body;
  const points = Array.from(body);
  return points.length > max ? `${points.slice(0, max).join("")}…` : body;
}
