import { describe, expect, it } from "vitest";
import { excerptBody } from "../src/excerpt";
import { apiJson, postJson } from "./helpers";

/** Compare through `JSON.stringify` so a failure prints a lone surrogate as an ASCII
 * escape; the raw character corrupts the test runner's own websocket. */
function same(actual: string, expected: string): void {
  expect(JSON.stringify(actual)).toBe(JSON.stringify(expected));
}

/** True when `text` has a surrogate half with no partner: the shape that
 * `JSON.stringify` writes as a lone `\udXXX` escape and `serde_json` rejects. */
function hasLoneSurrogate(text: string): boolean {
  return /[\ud800-\udbff](?![\udc00-\udfff])|(?<![\ud800-\udbff])[\udc00-\udfff]/.test(text);
}

describe("excerptBody", () => {
  // Why: native `output::excerpt_body` counts Unicode scalar values, so a
  // client that asks for N characters must get the same N from either tier.
  it("counts Unicode scalar values, not UTF-16 code units", () => {
    same(excerptBody("😀😀😀😀", 2), "😀😀…");
    same(excerptBody("😀😀", 2), "😀😀");
    same(excerptBody("abc", 3), "abc");
    same(excerptBody("abcd", 3), "abc…");
    same(excerptBody("", 0), "");
  });

  // Why: the bug from agent-hub#231. 199 ASCII characters then an emoji, cut at
  // 200, put the cut between the emoji's two UTF-16 halves with `slice`.
  it("never splits a surrogate pair at the cut", () => {
    const body = `${"a".repeat(199)}😀tail`;
    const cut = excerptBody(body, 200);
    same(cut, `${"a".repeat(199)}😀…`);
    expect(hasLoneSurrogate(cut)).toBe(false);
    // The old behaviour, kept here so the failure it caused stays visible.
    expect(hasLoneSurrogate(body.slice(0, 200))).toBe(true);
  });

  // Why: one boundary is not enough; every cut position around non-BMP
  // characters must match the native cut and stay well formed.
  it("agrees with the native cut for every boundary around a non-BMP character", () => {
    const body = "ab😀cd😀ef";
    const points = Array.from(body);
    for (let max = 0; max <= points.length + 1; max++) {
      const expected = points.length > max ? `${points.slice(0, max).join("")}…` : body;
      same(excerptBody(body, max), expected);
      expect(hasLoneSurrogate(excerptBody(body, max))).toBe(false);
    }
  });
});

describe("GET /messages?excerpt=", () => {
  // Why: the Rust client parses this response with serde_json; a lone surrogate
  // makes it reject the whole body, so the end-to-end response must stay valid.
  it("returns parseable JSON when the cut falls inside a non-BMP character", async () => {
    const recipient = "excerpt-surrogate-reader";
    const prefix = "STATUS: ";
    const padding = "a".repeat(200 - prefix.length - 1);
    const body = `${prefix}${padding}😀 and a tail past the cut`;
    await postJson("/messages", { sender: "claude", recipient, topic: "status", body });

    const { status, body: messages } = await apiJson<Array<{ body: string }>>(
      `/messages?agent=${recipient}&broadcast=false&excerpt=200`,
    );
    expect(status).toBe(200);
    expect(messages).toHaveLength(1);
    const excerpt = messages[0]?.body ?? "";
    same(excerpt, `${prefix}${padding}😀…`);
    expect(hasLoneSurrogate(excerpt)).toBe(false);
  });
});
