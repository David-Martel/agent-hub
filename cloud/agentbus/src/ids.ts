/** ID and timestamp helpers matching the Rust hub's formats exactly. */

/**
 * Generate a UUIDv7 (RFC 9562): 48-bit big-endian Unix ms timestamp, then
 * version/variant bits, then 74 random bits. Rust's `Uuid::now_v7()` is what
 * `agent-bus-core::channels::xadd_to_stream` and friends use for message ids;
 * `crypto.randomUUID()` only produces v4, so this is a from-scratch
 * implementation rather than a library dependency.
 */
export function uuidv7(): string {
  const bytes = new Uint8Array(16);
  const now = BigInt(Date.now());
  bytes[0] = Number((now >> 40n) & 0xffn);
  bytes[1] = Number((now >> 32n) & 0xffn);
  bytes[2] = Number((now >> 24n) & 0xffn);
  bytes[3] = Number((now >> 16n) & 0xffn);
  bytes[4] = Number((now >> 8n) & 0xffn);
  bytes[5] = Number(now & 0xffn);

  const rand = new Uint8Array(10);
  crypto.getRandomValues(rand);
  // `rand` has a fixed length of 10, so every index below is in bounds;
  // the `!` assertions only silence `noUncheckedIndexedAccess`.
  // Byte 6: version (0111) in the high nibble, 4 random bits in the low nibble.
  bytes[6] = 0x70 | (rand[0]! & 0x0f);
  // Byte 7: 8 more random bits.
  bytes[7] = rand[1]!;
  // Byte 8: variant (10) in the top two bits, 6 random bits.
  bytes[8] = 0x80 | (rand[2]! & 0x3f);
  bytes[9] = rand[3]!;
  bytes[10] = rand[4]!;
  bytes[11] = rand[5]!;
  bytes[12] = rand[6]!;
  bytes[13] = rand[7]!;
  bytes[14] = rand[8]!;
  bytes[15] = rand[9]!;

  const hex = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/**
 * Format a timestamp the way the Rust hub does:
 * `%Y-%m-%dT%H:%M:%S%.6fZ` — always exactly 6 fractional-second digits.
 * `Date` only carries millisecond precision, so the low 3 digits of the
 * fraction are always `000`; this is a formatting-compatibility shim, not a
 * claim of microsecond precision.
 */
export function formatTimestampUtc(date: Date = new Date()): string {
  const iso = date.toISOString(); // "2026-09-27T12:00:00.123Z"
  return iso.replace(/\.(\d{3})Z$/, ".$1000Z");
}

/** Parse a `formatTimestampUtc`-shaped (or plain ISO-8601) string to epoch ms. */
export function parseTimestampUtcMs(ts: string): number | null {
  const t = Date.parse(ts.replace(/(\.\d{3})\d{3}Z$/, "$1Z"));
  return Number.isNaN(t) ? null : t;
}

/**
 * Hybrid logical clock string: `<millis>-<counter>-<originHub>`. Cheap,
 * monotonic-enough tie-breaker for cross-hub ordering; `origin_seq` (a true
 * per-origin monotonic counter kept in the DO) is the authoritative order key.
 */
export function makeHlc(nowMs: number, counter: number, originHub: string): string {
  return `${nowMs}-${counter}-${originHub}`;
}
