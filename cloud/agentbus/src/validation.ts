/**
 * Input validation mirroring `agent-bus-core::validation`.
 *
 * Error messages are kept close to the Rust wording so existing CLI/MCP
 * clients that pattern-match on error text keep working unmodified.
 */

import type { JsonValue } from "./types";
import { parseTimestampUtcMs } from "./ids";

export const MAX_TOPIC_LEN = 256;
/** 256 KB, mirrors `agent_bus_core::validation::MAX_BODY_LEN`. */
export const MAX_BODY_LEN = 262_144;

export const VALID_PRIORITIES = ["low", "normal", "high", "urgent"] as const;

export class ValidationError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ValidationError";
  }
}

/** Mapped to HTTP 403 by `guarded()` in `index.ts` (agent-hub#82 security
 * review, H1/H2). Thrown when an authenticated caller's token identity
 * disagrees with a value it tried to assert in the request body/path. */
export class ForbiddenError extends Error {
  constructor(message: string) {
    super(message);
    this.name = "ForbiddenError";
  }
}

export function validatePriority(p: string): void {
  if (!(VALID_PRIORITIES as readonly string[]).includes(p)) {
    throw new ValidationError(
      `invalid priority '${p}'; must be one of: ${VALID_PRIORITIES.join(", ")}`,
    );
  }
}

export function rejectNulBytes(val: string, name: string): void {
  if (val.includes("\u0000")) {
    throw new ValidationError(`${name} must not contain NUL bytes (\\x00)`);
  }
}

export function nonEmpty(val: string, name: string): string {
  const trimmed = val.trim();
  if (trimmed.length === 0) {
    throw new ValidationError(`${name} must not be empty`);
  }
  if (name === "topic" && trimmed.length > MAX_TOPIC_LEN) {
    throw new ValidationError(`${name} exceeds maximum length of ${MAX_TOPIC_LEN}`);
  }
  if (name === "body" && trimmed.length > MAX_BODY_LEN) {
    throw new ValidationError(`${name} exceeds maximum length of ${MAX_BODY_LEN}`);
  }
  if (name === "topic" || name === "body") {
    rejectNulBytes(trimmed, name);
  }
  return trimmed;
}

export const SCHEMA_FINDING = "finding";
export const SCHEMA_STATUS = "status";
export const SCHEMA_BENCHMARK = "benchmark";

/** Mirrors `agent_bus_core::validation::validate_message_schema`. */
export function validateMessageSchema(body: string, schema: string | null | undefined): void {
  if (!schema) return;
  switch (schema) {
    case SCHEMA_FINDING: {
      if (
        !body.includes("FINDING:") &&
        !body.includes("FIX") &&
        !body.includes("TAGGED:") &&
        !body.includes("COMPLETE")
      ) {
        throw new ValidationError(
          "Schema 'finding' requires FINDING:, FIX, TAGGED:, or COMPLETE in body",
        );
      }
      if (body.includes("FINDING:") && !body.includes("SEVERITY:")) {
        throw new ValidationError("Schema 'finding' requires SEVERITY: when FINDING: is present");
      }
      return;
    }
    case SCHEMA_STATUS: {
      if (body.trim().length === 0) {
        throw new ValidationError("Schema 'status' requires non-empty body");
      }
      return;
    }
    case SCHEMA_BENCHMARK: {
      if (!body.includes("=")) {
        throw new ValidationError("Schema 'benchmark' requires key=value metrics in body");
      }
      return;
    }
    default:
      // Unknown explicit schema: the Rust side resolves this via
      // `enforce_schema_for_transport` before validation ever sees it, so an
      // unrecognised schema here is treated as "no schema" (same fallback the
      // CLI transport gets).
      return;
  }
}

/**
 * Mirrors `agent_bus_core::validation::enforce_schema_for_transport` for the
 * two transports the cloud tier accepts requests from (`http` direct callers,
 * and `sync` batch pushes from an origin hub — both behave like `"http"`).
 */
export function enforceSchemaForTransport(
  explicitSchema: string | null | undefined,
  topic: string,
): string | null {
  if (explicitSchema === SCHEMA_FINDING || explicitSchema === SCHEMA_STATUS || explicitSchema === SCHEMA_BENCHMARK) {
    return explicitSchema;
  }
  const t = topic.toLowerCase();
  if (t.includes("findings") || t.startsWith("review")) return SCHEMA_FINDING;
  if (["status", "ownership", "coordination", "handoff"].includes(t)) return SCHEMA_STATUS;
  if (t === "benchmark") return SCHEMA_BENCHMARK;
  // HTTP transport defaults unresolved topics to "status".
  return SCHEMA_STATUS;
}

/** Mirrors `agent_bus_core::validation::auto_fit_schema` (+ its two private
 * `auto_fit_finding`/`auto_fit_benchmark` helpers). */
export function autoFitSchema(body: string, schema: string | null | undefined): string {
  if (!schema) return body;
  if (schema === SCHEMA_FINDING) return autoFitFinding(body);
  if (schema === SCHEMA_BENCHMARK) return autoFitBenchmark(body);
  return body;
}

function autoFitFinding(body: string): string {
  if (body.includes("FINDING:") || body.includes("FIX") || body.includes("COMPLETE")) {
    return body;
  }
  const upper = body.toUpperCase();
  let severity: string;
  if (upper.includes("CRITICAL")) severity = "CRITICAL";
  else if (upper.includes("HIGH") || upper.includes("SECURITY") || upper.includes("VULNERABILITY"))
    severity = "HIGH";
  else if (upper.includes("MEDIUM") || upper.includes("WARNING")) severity = "MEDIUM";
  else severity = "LOW";
  return `FINDING: ${body}\nSEVERITY: ${severity}`;
}

function autoFitBenchmark(body: string): string {
  if (body.includes("=")) return body;
  return `summary=${body}`;
}

// ---------------------------------------------------------------------------
// agent-hub#79: sensitivity guard
// ---------------------------------------------------------------------------

export type Sensitivity = "internal" | "no-offsite";

export function validateSensitivity(value: unknown): Sensitivity {
  if (value === undefined || value === null) return "internal";
  if (value === "internal" || value === "no-offsite") return value;
  throw new ValidationError(`invalid sensitivity '${String(value)}'; expected internal|no-offsite`);
}

/**
 * The bus must never carry PHI (agent-hub#79 constraint, from
 * DTMVentures/headscale-ops policy). This is a coarse, high-recall,
 * necessarily-incomplete heuristic screen — NOT a HIPAA Safe Harbor
 * de-identification check — that rejects the most obvious identifiers before
 * they leave the lab network. It is defense in depth on top of the
 * `sensitivity: "no-offsite"` opt-out, never a substitute for it.
 */
const PHI_PATTERNS: RegExp[] = [
  /\b\d{3}-\d{2}-\d{4}\b/, // SSN
  /\bMRN[:#]?\s*\d{4,}\b/i, // medical record number
  /\bDOB[:#]?\s*\d{1,2}[/-]\d{1,2}[/-]\d{2,4}\b/i, // date of birth label
  /\bpatient\s+(name|id)\b/i,
];

export function containsObviousPhi(body: string): boolean {
  return PHI_PATTERNS.some((re) => re.test(body));
}

/** Runs the PHI screen over every text fragment given (metadata/tags should
 * be pre-serialized to JSON strings by the caller). Skips `undefined`/empty
 * fragments. Mirrors agent-hub#82 review item M7: the screen previously
 * covered only the `/messages` body. */
export function assertNoPhi(...texts: Array<string | undefined | null>): void {
  for (const text of texts) {
    if (text && containsObviousPhi(text)) {
      throw new ValidationError(
        "content matches an obvious PHI pattern (SSN/MRN/DOB/patient-identifier) and was rejected; the bus must never carry PHI",
      );
    }
  }
}

// ---------------------------------------------------------------------------
// agent-hub#82 security review: tags/metadata/size/timestamp validation
// ---------------------------------------------------------------------------

export const MAX_TAGS_COUNT = 64;
export const MAX_TAG_LEN = 256;
/** 64 KB per-message metadata cap (review M1/P7 — a 1.5-2.5 MB metadata blob
 * was accepted in Miniflare with no cap at all). */
export const MAX_METADATA_BYTES = 65_536;
/** Defense-in-depth cap on body + metadata + tags combined (review M1). */
export const MAX_TOTAL_MESSAGE_BYTES = 400_000;
/** Cap on a claim/renew lease TTL in seconds (review M4): 1 second to 24h. */
export const MAX_LEASE_TTL_SECONDS = 86_400;
export const MIN_LEASE_TTL_SECONDS = 1;
/** Cap on resource-name length used for claim DOs (review M6/M9). */
export const MAX_RESOURCE_NAME_LEN = 256;

/** `tags` MUST be `string[]` on every write path (review H7 — an untyped
 * `tags` value corrupted `JSON.parse(...).includes(...)` reads for the
 * lifetime of the row, with no delete route to recover). Throws on anything
 * that isn't an array of reasonably-sized, NUL-free strings. */
export function validateTags(value: unknown): string[] {
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) {
    throw new ValidationError("tags must be an array of strings");
  }
  if (value.length > MAX_TAGS_COUNT) {
    throw new ValidationError(`tags exceeds maximum count of ${MAX_TAGS_COUNT}`);
  }
  return value.map((t, i) => {
    if (typeof t !== "string") {
      throw new ValidationError(`tags[${i}] must be a string`);
    }
    if (t.length > MAX_TAG_LEN) {
      throw new ValidationError(`tags[${i}] exceeds maximum length of ${MAX_TAG_LEN}`);
    }
    rejectNulBytes(t, `tags[${i}]`);
    return t;
  });
}

/** Serializes `metadata` (if present) and rejects it once it exceeds
 * `MAX_METADATA_BYTES`. Returns the byte length actually measured (0 when
 * `metadata` is absent) so callers can fold it into a total-size check
 * without re-serializing. */
export function validateMetadataSize(metadata: JsonValue | undefined): number {
  if (metadata === undefined || metadata === null) return 0;
  const size = new TextEncoder().encode(JSON.stringify(metadata)).length;
  if (size > MAX_METADATA_BYTES) {
    throw new ValidationError(`metadata exceeds maximum size of ${MAX_METADATA_BYTES} bytes`);
  }
  return size;
}

/** Defense-in-depth total-size cap across body + metadata + tags, on top of
 * the per-field caps above (review M1). */
export function validateTotalSize(bodyLen: number, metadataBytes: number, tags: string[]): void {
  const tagsBytes = tags.reduce((acc, t) => acc + t.length, 0);
  const total = bodyLen + metadataBytes + tagsBytes;
  if (total > MAX_TOTAL_MESSAGE_BYTES) {
    throw new ValidationError(`message exceeds maximum total size of ${MAX_TOTAL_MESSAGE_BYTES} bytes`);
  }
}

/** Validates a caller-supplied UTC timestamp string (used for historical
 * `/sync/push` imports, which are allowed to keep their original
 * `timestamp_utc` — review item 2). Rejects anything that doesn't parse and
 * anything implausibly far in the future (a year-9999 timestamp broke
 * `ClaimDO`'s string-comparison expiry logic in the review's P3/P8 probes).
 * Past dates are always accepted — that's the whole point of a historical
 * import. */
export function validateTimestampUtc(ts: string, name = "timestamp_utc"): string {
  const ms = parseTimestampUtcMs(ts);
  if (ms === null) {
    throw new ValidationError(`${name} is not a valid ISO-8601 UTC timestamp`);
  }
  const maxFutureMs = Date.now() + 365 * 24 * 60 * 60 * 1000;
  if (ms > maxFutureMs) {
    throw new ValidationError(`${name} is too far in the future`);
  }
  return ts;
}

/** Validates a caller-supplied `protocol_version` string (historical imports
 * carry their own). Kept intentionally loose — `"1.0"`, `"1"`, `"2.1"`. */
export function validateProtocolVersion(v: string): string {
  if (!/^\d+(\.\d+)*$/.test(v)) {
    throw new ValidationError("protocol_version must look like a dotted version number");
  }
  return v;
}

/**
 * Parse a query-string integer parameter. Non-numeric input is a 400
 * (review L1/L2/P8 — `Number("abc")` propagating into SQLite bind
 * parameters produced a raw `SQLITE_MISMATCH` 500, and `since=abc` on
 * `/sync/pull` silently returned an empty page instead of failing). A
 * numeric value outside `[min, max]` is clamped, matching the existing
 * (intentional, review-approved) clamping behavior for query params like
 * `history`.
 */
export function parseIntParam(
  raw: string | undefined | null,
  name: string,
  opts: { min?: number; max?: number; default?: number } = {},
): number {
  if (raw === undefined || raw === null || raw === "") {
    if (opts.default !== undefined) return opts.default;
    throw new ValidationError(`${name} is required`);
  }
  if (!/^-?\d+$/.test(raw)) {
    throw new ValidationError(`${name} must be an integer`);
  }
  let n = Number(raw);
  if (!Number.isSafeInteger(n)) {
    throw new ValidationError(`${name} is out of range`);
  }
  if (opts.min !== undefined) n = Math.max(n, opts.min);
  if (opts.max !== undefined) n = Math.min(n, opts.max);
  return n;
}

/** Same idea as `parseIntParam` but for a JSON body field, which arrives as
 * `unknown` rather than a query string (review M4/P3: `lease_ttl_seconds:
 * "abc"` or `1e13` produced a 500 "Invalid time value" deep inside
 * `formatTimestampUtc`, not a 400 at the boundary). `undefined`/`null`
 * return `undefined` so the caller can apply its own default. */
export function coerceOptionalInt(
  value: unknown,
  name: string,
  opts: { min?: number; max?: number } = {},
): number | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "number" || !Number.isFinite(value) || !Number.isInteger(value)) {
    throw new ValidationError(`${name} must be an integer`);
  }
  let n = value;
  if (opts.min !== undefined) n = Math.max(n, opts.min);
  if (opts.max !== undefined) n = Math.min(n, opts.max);
  return n;
}

// ---------------------------------------------------------------------------
// agent-hub#82: shared core message validation ("the same validator as
// /messages", applied to /sync/push too — see SYNC-CONTRACT.md for the one
// documented deviation: schema auto-fit is NOT applied to synced items).
// ---------------------------------------------------------------------------

export interface RawMessageFields {
  sender?: string;
  recipient?: string;
  topic?: string;
  body?: string;
  tags?: unknown;
  priority?: string;
  metadata?: JsonValue;
  sensitivity?: unknown;
}

export interface CoreMessageFields {
  sender: string;
  recipient: string;
  topic: string;
  body: string;
  tags: string[];
  priority: string;
  metadata: JsonValue;
  sensitivity: Sensitivity;
}

/**
 * The validation every message-shaped write path shares: non-empty/length/
 * NUL-safe sender-recipient-topic-body, a valid priority, a well-typed
 * `tags` array, a fail-closed `sensitivity` check (rejecting
 * `no-offsite` outright — the cloud tier never stores it), metadata/total
 * size caps, and the PHI screen over body + metadata + tags.
 *
 * Schema auto-fit/validation (`enforceSchemaForTransport` /
 * `autoFitSchema` / `validateMessageSchema`) is deliberately NOT part of
 * this shared core — see `buildValidatedMessage` in `index.ts` for the
 * direct-post path, which layers it on top for `/messages` only. Synced
 * historical rows keep their original body untouched.
 */
export function validateMessageCore(req: RawMessageFields): CoreMessageFields {
  const priority = req.priority ?? "normal";
  validatePriority(priority);
  const sender = nonEmpty(req.sender ?? "", "sender");
  const recipient = nonEmpty(req.recipient ?? "", "recipient");
  const topic = nonEmpty(req.topic ?? "", "topic");
  const body = nonEmpty(req.body ?? "", "body");
  const tags = validateTags(req.tags);

  const sensitivity = validateSensitivity(req.sensitivity);
  if (sensitivity === "no-offsite") {
    throw new ValidationError(
      "sensitivity=no-offsite messages are never accepted by the cloud tier (post them to the on-site hub only)",
    );
  }

  const metadataBytes = validateMetadataSize(req.metadata);
  validateTotalSize(body.length, metadataBytes, tags);

  assertNoPhi(body, req.metadata !== undefined ? JSON.stringify(req.metadata) : undefined, tags.join(" "));

  return { sender, recipient, topic, body, tags, priority, metadata: req.metadata ?? {}, sensitivity };
}
