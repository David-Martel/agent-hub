/**
 * Input validation mirroring `agent-bus-core::validation`.
 *
 * Error messages are kept close to the Rust wording so existing CLI/MCP
 * clients that pattern-match on error text keep working unmodified.
 */

import type { JsonValue } from "./types";
import { formatTimestampUtc, parseTimestampUtcMs } from "./ids";

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

/**
 * Non-empty, NUL-free, optionally length-capped string. `maxLen` used to be
 * implicit and name-keyed (only "topic"/"body" were ever capped) — agent-hub
 * #82 re-review N2 found every OTHER required field (`sender`, `recipient`)
 * had no cap at all, so a 1 MB `recipient` was accepted verbatim. Every
 * caller now passes its own cap explicitly.
 */
export function nonEmpty(val: string, name: string, maxLen?: number): string {
  const trimmed = val.trim();
  if (trimmed.length === 0) {
    throw new ValidationError(`${name} must not be empty`);
  }
  if (maxLen !== undefined && trimmed.length > maxLen) {
    throw new ValidationError(`${name} exceeds maximum length of ${maxLen}`);
  }
  rejectNulBytes(trimmed, name);
  return trimmed;
}

/**
 * An OPTIONAL string field: type-checked, NUL-free, length-capped. An
 * absent/`null` value, or a value that trims to empty, returns `undefined`
 * (empty and omitted are treated the same, matching the identity-binding
 * `bind*` helpers in `auth.ts`). Anything present but not a string is
 * rejected outright, closing agent-hub#82 re-review N3/N4 (`namespace:{}`,
 * `session_id:{}` and similar previously either 500'd deep in a DO method or
 * were silently coerced to `"[object Object]"` by `String()`/template
 * interpolation).
 */
export function validateOptionalString(value: unknown, name: string, maxLen: number): string | undefined {
  if (value === undefined || value === null) return undefined;
  if (typeof value !== "string") {
    throw new ValidationError(`${name} must be a string`);
  }
  const trimmed = value.trim();
  if (trimmed.length === 0) return undefined;
  if (trimmed.length > maxLen) {
    throw new ValidationError(`${name} exceeds maximum length of ${maxLen}`);
  }
  rejectNulBytes(trimmed, name);
  return trimmed;
}

/** Same as `validateOptionalString`, but returns `fallback` instead of
 * `undefined` for an absent/empty value — for fields like presence `status`
 * that always have a meaningful default. */
export function requireStringOr(value: unknown, name: string, maxLen: number, fallback: string): string {
  return validateOptionalString(value, name, maxLen) ?? fallback;
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
/**
 * Defense-in-depth cap on the ENTIRE row's string content combined (review
 * M1, tightened by re-review N2). Comfortably under Cloudflare's documented
 * ~2 MB per-row limit for Durable Object SQLite storage (with per-field caps
 * below providing the real ceiling — this total is a backstop against many
 * small-but-not-individually-capped fields adding up, not the primary
 * defense).
 */
export const MAX_TOTAL_MESSAGE_BYTES = 400_000;
/** Cloudflare's documented approximate per-row limit for Durable Object
 * SQLite storage (agent-hub#82 re-review N2). `MAX_TOTAL_MESSAGE_BYTES` is
 * kept at 5x margin below this, in code (not just in a comment) — see
 * `test/security.test.ts`'s "row size margin" assertion, which fails loudly
 * if a future edit narrows that margin without noticing. */
export const DO_SQLITE_ROW_LIMIT_BYTES = 2_000_000;
/** Cap on a claim/renew lease TTL in seconds (review M4): 1 second to 24h. */
export const MAX_LEASE_TTL_SECONDS = 86_400;
export const MIN_LEASE_TTL_SECONDS = 1;
/** Cap on resource-name length used for claim DOs (review M6/M9). */
export const MAX_RESOURCE_NAME_LEN = 256;

// ---------------------------------------------------------------------------
// agent-hub#82 re-review N2/N3/N4: every remaining string/array field gets a
// type check and a length cap. Sizes are deliberately small — these are all
// identifiers, hostnames, or short labels, never user-facing prose.
// ---------------------------------------------------------------------------

export const MAX_SENDER_LEN = 256;
export const MAX_RECIPIENT_LEN = 256;
export const MAX_THREAD_ID_LEN = 256;
export const MAX_REPLY_TO_LEN = 128;
export const MAX_CLIENT_MSG_ID_LEN = 128;
export const MAX_HLC_LEN = 128;
export const MAX_ORIGIN_HOST_LEN = 256;
export const MAX_ORIGIN_HUB_LEN = 256;
/** Sync item `id` (review N2's "sender on sync" family; `id` itself was
 * already capped inline in `index.ts` — centralized here). */
export const MAX_SYNC_ID_LEN = 128;

export const MAX_STATUS_LEN = 64;
export const MAX_SESSION_ID_LEN = 256;

export const MAX_NAMESPACE_LEN = 256;
export const MAX_PRIORITY_ARGUMENT_LEN = 512;
export const MAX_SCOPE_KIND_LEN = 128;
export const MAX_SCOPE_PATH_LEN = 1024;
export const MAX_CLAIM_THREAD_ID_LEN = 256;

/**
 * `value` MUST be `string[]` (review H7, extended by re-review N3/N4 to
 * `capabilities` and `repo_scopes` — the same class of bug: an untyped array
 * field corrupts every future reader for as long as the poisoned row
 * survives). `fieldName` is used in every error message so `capabilities[0]`
 * / `repo_scopes[0]` read naturally instead of a generic `tags[0]`.
 */
export function validateStringArray(
  value: unknown,
  fieldName: string,
  opts: { maxCount?: number; maxItemLen?: number } = {},
): string[] {
  const maxCount = opts.maxCount ?? MAX_TAGS_COUNT;
  const maxItemLen = opts.maxItemLen ?? MAX_TAG_LEN;
  if (value === undefined || value === null) return [];
  if (!Array.isArray(value)) {
    throw new ValidationError(`${fieldName} must be an array of strings`);
  }
  if (value.length > maxCount) {
    throw new ValidationError(`${fieldName} exceeds maximum count of ${maxCount}`);
  }
  return value.map((t, i) => {
    if (typeof t !== "string") {
      throw new ValidationError(`${fieldName}[${i}] must be a string`);
    }
    if (t.length > maxItemLen) {
      throw new ValidationError(`${fieldName}[${i}] exceeds maximum length of ${maxItemLen}`);
    }
    rejectNulBytes(t, `${fieldName}[${i}]`);
    return t;
  });
}

/** `tags` MUST be `string[]` on every write path (review H7). Thin wrapper
 * over `validateStringArray` kept for the error-message text existing
 * clients/tests already match on. */
export function validateTags(value: unknown): string[] {
  return validateStringArray(value, "tags");
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

/**
 * Defense-in-depth total-size cap across EVERY string field on the row plus
 * metadata, on top of the per-field caps above (review M1, tightened by
 * re-review N2: `thread_id`, `reply_to`, `client_msg_id`, `hlc`, `recipient`,
 * `origin_host`/`origin_hub` and `sender` used to be completely excluded
 * from this accounting, so a hub push of several uncapped 1 MB fields at
 * once summed to multiple megabytes despite the "total" cap existing).
 * `parts` takes every string-valued field on the row; falsy entries are
 * skipped so callers can pass optional fields directly.
 */
export function validateTotalSize(parts: Array<string | undefined>, metadataBytes: number): void {
  let total = metadataBytes;
  for (const part of parts) {
    if (part) total += part.length;
  }
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
  // Normalize to the canonical `formatTimestampUtc` shape (agent-hub#82
  // re-review N10): `Date.parse` (which `parseTimestampUtcMs` wraps) is far
  // more permissive than the shape every OTHER row uses — a space separator
  // or a non-`Z` offset (e.g. "2020-01-01 00:00:00+05:00") parses fine but
  // sorts and string-compares incorrectly against canonical rows, making
  // such a row invisible to a `since=<minutes>` cutoff query and unreliable
  // for retention/presence "is this newer" comparisons. Returning the
  // re-formatted value (rather than rejecting non-canonical input outright)
  // is the more forgiving of the two options the review offered, and is
  // friendlier to the not-yet-written Rust sync client.
  return formatTimestampUtc(new Date(ms));
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
  thread_id?: string;
  reply_to?: string;
  client_msg_id?: string;
  hlc?: string;
}

export interface CoreMessageFields {
  /** Empty string means "not provided" (agent-hub#82 re-review N9): unlike
   * recipient/topic/body, `sender` is NOT required here — a direct
   * `/messages` post fills an omitted sender from the caller's own
   * identity (`bindAgent` in `auth.ts`), while `/sync/push` requires it
   * explicitly (there's no "identity" for a hub-vouched historical row to
   * default to) — see `buildSyncPushItem` in `index.ts`. */
  sender: string;
  recipient: string;
  topic: string;
  body: string;
  tags: string[];
  priority: string;
  metadata: JsonValue;
  sensitivity: Sensitivity;
  thread_id?: string;
  reply_to?: string;
  client_msg_id?: string;
  hlc?: string;
}

/**
 * The validation every message-shaped write path shares: length/NUL-safe
 * recipient-topic-body (required) and sender (optional — see
 * `CoreMessageFields.sender`'s docs), every optional side field
 * (`thread_id`/`reply_to`/`client_msg_id`/`hlc`) type-checked and capped, a
 * valid priority, a well-typed `tags` array, a fail-closed `sensitivity`
 * check (rejecting `no-offsite` outright — the cloud tier never stores it),
 * metadata/total size caps across EVERY field, and the PHI screen over body,
 * metadata, tags, topic, thread_id and recipient (re-review M7/N2: `topic`,
 * `thread_id` and `recipient` were unscreened before).
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
  const sender = validateOptionalString(req.sender, "sender", MAX_SENDER_LEN) ?? "";
  const recipient = nonEmpty(req.recipient ?? "", "recipient", MAX_RECIPIENT_LEN);
  const topic = nonEmpty(req.topic ?? "", "topic", MAX_TOPIC_LEN);
  const body = nonEmpty(req.body ?? "", "body", MAX_BODY_LEN);
  const tags = validateTags(req.tags);
  const threadId = validateOptionalString(req.thread_id, "thread_id", MAX_THREAD_ID_LEN);
  const replyTo = validateOptionalString(req.reply_to, "reply_to", MAX_REPLY_TO_LEN);
  const clientMsgId = validateOptionalString(req.client_msg_id, "client_msg_id", MAX_CLIENT_MSG_ID_LEN);
  const hlc = validateOptionalString(req.hlc, "hlc", MAX_HLC_LEN);

  const sensitivity = validateSensitivity(req.sensitivity);
  if (sensitivity === "no-offsite") {
    throw new ValidationError(
      "sensitivity=no-offsite messages are never accepted by the cloud tier (post them to the on-site hub only)",
    );
  }

  const metadataBytes = validateMetadataSize(req.metadata);
  validateTotalSize(
    [body, sender, recipient, topic, threadId, replyTo, clientMsgId, hlc, ...tags],
    metadataBytes,
  );

  assertNoPhi(
    body,
    req.metadata !== undefined ? JSON.stringify(req.metadata) : undefined,
    tags.join(" "),
    topic,
    threadId,
    recipient,
  );

  return {
    sender,
    recipient,
    topic,
    body,
    tags,
    priority,
    metadata: req.metadata ?? {},
    sensitivity,
    thread_id: threadId,
    reply_to: replyTo,
    client_msg_id: clientMsgId,
    hlc,
  };
}

// ---------------------------------------------------------------------------
// agent-hub#82 re-review N3: presence field validation (same class of bug as
// H7 — `capabilities`, `status`, `session_id`, `network_context` and
// `metadata` were all accepted with no type check or size cap).
// ---------------------------------------------------------------------------

/** Mirrors `NetworkContext` in `types.ts`. Duplicated here (rather than
 * imported) to avoid a validation.ts -> types.ts -> validation.ts cycle;
 * kept in sync by hand — if the Rust/wire side adds a fifth value first,
 * this enum-validation is exactly where it needs updating too. */
const VALID_NETWORK_CONTEXTS = ["lab-p2p", "lab-lan", "offsite", "offline"] as const;

export interface RawPresenceFields {
  status?: unknown;
  session_id?: unknown;
  capabilities?: unknown;
  metadata?: JsonValue;
  network_context?: unknown;
}

export interface CorePresenceFields {
  status: string;
  session_id: string;
  capabilities: string[];
  metadata: JsonValue;
  network_context?: string;
}

/**
 * Shared by `PUT /presence/:agent` and each event in `POST
 * /sync/push-presence` (re-review N3). `status` defaults to `"online"` and
 * `session_id` to `""` when omitted, matching the pre-validation behavior;
 * either one, if PROVIDED, must be a string within its cap. `capabilities`
 * reuses the same array-of-strings check as `tags`/`repo_scopes`.
 * `network_context`, if present, must be one of the four known values.
 */
export function validatePresenceCore(req: RawPresenceFields): CorePresenceFields {
  const status = requireStringOr(req.status, "status", MAX_STATUS_LEN, "online");
  const sessionId = requireStringOr(req.session_id, "session_id", MAX_SESSION_ID_LEN, "");
  const capabilities = validateStringArray(req.capabilities, "capabilities");
  const metadataBytes = validateMetadataSize(req.metadata);
  validateTotalSize([status, sessionId, ...capabilities], metadataBytes);

  let networkContext: string | undefined;
  if (req.network_context !== undefined && req.network_context !== null) {
    if (typeof req.network_context !== "string") {
      throw new ValidationError("network_context must be a string");
    }
    if (!(VALID_NETWORK_CONTEXTS as readonly string[]).includes(req.network_context)) {
      throw new ValidationError(
        `invalid network_context '${req.network_context}'; must be one of: ${VALID_NETWORK_CONTEXTS.join(", ")}`,
      );
    }
    networkContext = req.network_context;
  }

  assertNoPhi(status, req.metadata !== undefined ? JSON.stringify(req.metadata) : undefined);

  return { status, session_id: sessionId, capabilities, metadata: req.metadata ?? {}, network_context: networkContext };
}

// ---------------------------------------------------------------------------
// agent-hub#82 re-review N4: claim field validation (`namespace:{}` 500'd,
// `repo_scopes:"abc"` was stored and served back as a bare string, a 1 MB
// `priority_argument` was accepted).
// ---------------------------------------------------------------------------

export interface RawClaimFields {
  priority_argument?: unknown;
  namespace?: unknown;
  scope_kind?: unknown;
  scope_path?: unknown;
  repo_scopes?: unknown;
  thread_id?: unknown;
}

export interface CoreClaimFields {
  priorityArgument: string;
  namespace?: string;
  scopeKind?: string;
  scopePath?: string;
  repoScopes: string[];
  threadId?: string;
}

export function validateClaimCore(req: RawClaimFields): CoreClaimFields {
  const priorityArgument = requireStringOr(
    req.priority_argument,
    "priority_argument",
    MAX_PRIORITY_ARGUMENT_LEN,
    "first-edit required",
  );
  const namespace = validateOptionalString(req.namespace, "namespace", MAX_NAMESPACE_LEN);
  const scopeKind = validateOptionalString(req.scope_kind, "scope_kind", MAX_SCOPE_KIND_LEN);
  const scopePath = validateOptionalString(req.scope_path, "scope_path", MAX_SCOPE_PATH_LEN);
  const repoScopes = validateStringArray(req.repo_scopes, "repo_scopes");
  const threadId = validateOptionalString(req.thread_id, "thread_id", MAX_CLAIM_THREAD_ID_LEN);
  return { priorityArgument, namespace, scopeKind, scopePath, repoScopes, threadId };
}
