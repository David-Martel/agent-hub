/**
 * Input validation mirroring `agent-bus-core::validation`.
 *
 * Error messages are kept close to the Rust wording so existing CLI/MCP
 * clients that pattern-match on error text keep working unmodified.
 */

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
