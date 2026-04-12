//! Input validation helpers.

use crate::error::{AgentBusError, Result};


pub const VALID_PRIORITIES: &[&str] = &["low", "normal", "high", "urgent"];
pub const MAX_TOPIC_LEN: usize = 256;
pub const MAX_BODY_LEN: usize = 10_485_760; // 10MB

/// # Errors
/// Returns an error if `p` is not one of the valid priority values.
pub fn validate_priority(p: &str) -> Result<()> {
    if VALID_PRIORITIES.contains(&p) {
        Ok(())
    } else {
        Err(AgentBusError::InvalidParams(format!(
            "invalid priority '{p}'; must be one of: {}",
            VALID_PRIORITIES.join(", ")
        )))
    }
}

/// # Errors
/// Returns an error if `val` is empty or only whitespace.
pub fn non_empty<'a>(val: &'a str, name: &str) -> Result<&'a str> {
    let trimmed = val.trim();
    if trimmed.is_empty() {
        Err(AgentBusError::InvalidParams(format!("{name} must not be empty")))
    } else if name == "topic" && trimmed.len() > MAX_TOPIC_LEN {
        Err(AgentBusError::InvalidParams(format!("{name} exceeds maximum length of {MAX_TOPIC_LEN}")))
    } else if name == "body" && trimmed.len() > MAX_BODY_LEN {
        Err(AgentBusError::InvalidParams(format!("{name} exceeds maximum length of {MAX_BODY_LEN}")))
    } else {
        Ok(trimmed)
    }
}

/// # Errors
/// Returns an error if `metadata` is `Some` but not valid JSON.
pub fn parse_metadata_arg(metadata: Option<&str>) -> Result<serde_json::Value> {
    match metadata {
        None => Ok(serde_json::Value::Object(serde_json::Map::new())),
        Some(s) => serde_json::from_str(s).map_err(|e| AgentBusError::InvalidParams(format!("--metadata must be valid JSON object: {e}"))),
    }
}

/// Known schema identifiers for structured message bodies.
pub const SCHEMA_FINDING: &str = "finding";
pub const SCHEMA_STATUS: &str = "status";
pub const SCHEMA_BENCHMARK: &str = "benchmark";

/// Validate a message body against a named schema.
///
/// Returns `Ok(())` when `schema` is `None` (no validation required) or when
/// the body satisfies the schema's keyword constraints. Returns an error with
/// a diagnostic message describing what the body must contain.
///
/// # Errors
///
/// Returns an error if the body does not satisfy the named schema, or if the
/// schema name is not one of `finding`, `status`, `benchmark`.
/// Infer schema from topic name when not explicitly provided.
///
/// Maps common topic patterns to their expected schema:
/// - `*-findings`, `review*` → finding
/// - `status`, `ownership`, `coordination`, `handoff` → status
/// - `benchmark`, `perf-*` → benchmark
#[must_use]
pub fn infer_schema_from_topic<'a>(
    topic: &str,
    explicit_schema: Option<&'a str>,
) -> Option<&'a str> {
    if explicit_schema.is_some() {
        return explicit_schema;
    }
    match topic {
        t if t.contains("findings") || t.starts_with("review") => Some("finding"),
        "status" | "ownership" | "coordination" | "handoff" => Some("status"),
        "benchmark" => Some("benchmark"),
        _ => None,
    }
}

/// Determine the effective schema for a message given the transport, an optional explicit
/// schema, and the topic.
///
/// Resolution order:
/// 1. If `explicit_schema` names a known schema (`finding`, `status`, `benchmark`), use it.
/// 2. Otherwise try topic-based inference via [`infer_schema_from_topic`].
/// 3. If transport is `"mcp"` or `"http"` and no schema was resolved, default to `"status"`.
/// 4. For `"cli"` and all other transports, return `None` (schema remains optional).
///
/// # Examples
///
/// ```
/// use agent_bus_core::validation::enforce_schema_for_transport;
/// // MCP always gets at least "status"
/// assert_eq!(enforce_schema_for_transport("mcp", None, "unknown"), Some("status"));
/// // CLI stays optional for unknown topics
/// assert_eq!(enforce_schema_for_transport("cli", None, "unknown"), None);
/// // Explicit schema is always honoured
/// assert_eq!(enforce_schema_for_transport("cli", Some("finding"), "anything"), Some("finding"));
/// ```
#[must_use]
pub fn enforce_schema_for_transport(
    transport: &str,
    explicit_schema: Option<&str>,
    topic: &str,
) -> Option<&'static str> {
    // Map an arbitrary &str to its canonical &'static str equivalent.
    let to_static = |s: &str| -> Option<&'static str> {
        match s {
            SCHEMA_FINDING => Some(SCHEMA_FINDING),
            SCHEMA_STATUS => Some(SCHEMA_STATUS),
            SCHEMA_BENCHMARK => Some(SCHEMA_BENCHMARK),
            _ => None,
        }
    };

    // 1. Explicit schema takes priority.
    if let Some(schema) = explicit_schema
        && let Some(s) = to_static(schema)
    {
        return Some(s);
    }
    // Unknown explicit schema: fall through so topic inference still runs.

    // 2. Topic-based inference (returns a &'static str via match).
    let inferred: Option<&'static str> = match topic {
        t if t.contains("findings") || t.starts_with("review") => Some(SCHEMA_FINDING),
        "status" | "ownership" | "coordination" | "handoff" => Some(SCHEMA_STATUS),
        "benchmark" => Some(SCHEMA_BENCHMARK),
        _ => None,
    };
    if inferred.is_some() {
        return inferred;
    }

    // 3. MCP/HTTP mandate at least a status schema.
    if transport == "mcp" || transport == "http" {
        return Some(SCHEMA_STATUS);
    }

    // 4. CLI and other transports: schema is optional.
    None
}

/// Auto-fit a message body to match the required schema format.
///
/// Returns the body unchanged when no schema is specified, or a cleaned
/// version that satisfies schema validation. This allows callers to send
/// plain-text bodies against schemas that have structural requirements
/// without failing validation.
///
/// # Examples
///
/// ```
/// use agent_bus_core::validation::auto_fit_schema;
/// let fitted = auto_fit_schema("memory leak in allocator", Some("finding"));
/// assert!(fitted.contains("FINDING:"));
/// assert!(fitted.contains("SEVERITY:"));
/// ```
#[must_use]
pub fn auto_fit_schema(body: &str, schema: Option<&str>) -> String {
    let Some(schema) = schema else {
        return body.to_owned();
    };
    match schema {
        SCHEMA_FINDING => auto_fit_finding(body),
        SCHEMA_BENCHMARK => auto_fit_benchmark(body),
        // status and unknown schemas: pass body through unchanged
        _ => body.to_owned(),
    }
}

fn auto_fit_finding(body: &str) -> String {
    // If body already has FINDING: or FIX or COMPLETE, return as-is.
    if body.contains("FINDING:") || body.contains("FIX") || body.contains("COMPLETE") {
        return body.to_owned();
    }
    // Detect severity from content keywords.
    let severity = if body.to_uppercase().contains("CRITICAL") {
        "CRITICAL"
    } else if body.to_uppercase().contains("HIGH")
        || body.to_uppercase().contains("SECURITY")
        || body.to_uppercase().contains("VULNERABILITY")
    {
        "HIGH"
    } else if body.to_uppercase().contains("MEDIUM") || body.to_uppercase().contains("WARNING") {
        "MEDIUM"
    } else {
        "LOW"
    };
    format!("FINDING: {body}\nSEVERITY: {severity}")
}

fn auto_fit_benchmark(body: &str) -> String {
    // If body already has key=value metrics, return as-is.
    if body.contains('=') {
        return body.to_owned();
    }
    format!("summary={body}")
}

/// # Errors
/// Returns an error if `body` does not satisfy the named schema constraints.
pub fn validate_message_schema(body: &str, schema: Option<&str>) -> Result<()> {
    let Some(schema) = schema else {
        return Ok(());
    };
    match schema {
        SCHEMA_FINDING => {
            // Must contain FINDING:, FIX, TAGGED:, or COMPLETE.
            if !body.contains("FINDING:")
                && !body.contains("FIX")
                && !body.contains("TAGGED:")
                && !body.contains("COMPLETE")
            {
                return Err(AgentBusError::InvalidParams("Schema 'finding' requires FINDING:, FIX, TAGGED:, or COMPLETE in body".to_owned()));
            }
            // When a FINDING: is declared, SEVERITY: must also be present.
            if body.contains("FINDING:") && !body.contains("SEVERITY:") {
                return Err(AgentBusError::InvalidParams("Schema 'finding' requires SEVERITY: when FINDING: is present".to_owned()));
            }
            Ok(())
        }
        SCHEMA_STATUS => {
            // Status messages are free-form but must be non-empty.
            if body.trim().is_empty() {
                return Err(AgentBusError::InvalidParams("Schema 'status' requires non-empty body".to_owned()));
            }
            Ok(())
        }
        SCHEMA_BENCHMARK => {
            // Must contain key=value metrics (any key, not just agents/msgs/duration).
            if !body.contains('=') {
                return Err(AgentBusError::InvalidParams("Schema 'benchmark' requires key=value metrics in body".to_owned()));
            }
            Ok(())
        }
        other => {
            Err(AgentBusError::InvalidParams(format!(
                "Unknown message schema: '{other}'. Valid: {SCHEMA_FINDING}, {SCHEMA_STATUS}, {SCHEMA_BENCHMARK}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_priority_accepts_all_valid_values() {
        for &p in VALID_PRIORITIES {
            assert!(validate_priority(p).is_ok(), "expected '{p}' to be valid");
        }
    }

    #[test]
    fn validate_priority_rejects_unknown() {
        assert!(validate_priority("critical").is_err());
        assert!(validate_priority("").is_err());
        assert!(validate_priority("NORMAL").is_err());
    }

    #[test]
    fn non_empty_returns_trimmed_value() {
        assert_eq!(non_empty("  hello  ", "field").unwrap(), "hello");
    }

    #[test]
    fn non_empty_rejects_blank() {
        assert!(non_empty("", "field").is_err());
        assert!(non_empty("   ", "field").is_err());
    }

    #[test]
    fn parse_metadata_arg_none_returns_empty_object() {
        let val = parse_metadata_arg(None).unwrap();
        assert!(val.is_object());
        assert!(val.as_object().unwrap().is_empty());
    }

    #[test]
    fn parse_metadata_arg_valid_json() {
        let val = parse_metadata_arg(Some(r#"{"key":"value"}"#)).unwrap();
        assert_eq!(val["key"], "value");
    }

    #[test]
    fn parse_metadata_arg_invalid_json() {
        assert!(parse_metadata_arg(Some("not json")).is_err());
    }

    #[test]
    fn validate_message_schema_none_always_passes() {
        assert!(validate_message_schema("anything", None).is_ok());
        assert!(validate_message_schema("", None).is_ok());
    }

    #[test]
    fn validate_message_schema_finding_accepts_valid_bodies() {
        for body in &[
            "FINDING: missing semicolon\nSEVERITY: LOW",
            "FIX the issue",
            "TAGGED: rust",
            "COMPLETE",
        ] {
            assert!(
                validate_message_schema(body, Some("finding")).is_ok(),
                "expected valid finding body: {body}"
            );
        }
    }

    #[test]
    fn validate_message_schema_finding_rejects_empty_body() {
        assert!(validate_message_schema("no keywords here", Some("finding")).is_err());
    }

    #[test]
    fn validate_message_schema_status_accepts_valid_body() {
        assert!(validate_message_schema("STATUS: online", Some("status")).is_ok());
    }

    #[test]
    fn validate_message_schema_status_rejects_empty() {
        assert!(validate_message_schema("", Some("status")).is_err());
        assert!(validate_message_schema("   ", Some("status")).is_err());
    }

    #[test]
    fn validate_message_schema_status_accepts_any_nonempty() {
        assert!(validate_message_schema("CLAIMING: file.py for task", Some("status")).is_ok());
        assert!(validate_message_schema("all good", Some("status")).is_ok());
    }

    #[test]
    fn validate_message_schema_benchmark_accepts_each_metric() {
        for body in &[
            "agents=3 complete",
            "msgs=100 processed",
            "duration=5.2s elapsed",
        ] {
            assert!(
                validate_message_schema(body, Some("benchmark")).is_ok(),
                "expected valid benchmark body: {body}"
            );
        }
    }

    #[test]
    fn validate_message_schema_benchmark_rejects_no_metrics() {
        assert!(validate_message_schema("no metrics at all", Some("benchmark")).is_err());
    }

    #[test]
    fn validate_message_schema_rejects_unknown_schema() {
        let err = validate_message_schema("body", Some("foobar")).unwrap_err();
        assert!(err.to_string().contains("Unknown message schema"));
    }

    // -----------------------------------------------------------------------
    // Task 7 tests — schema validation contract
    // -----------------------------------------------------------------------

    #[test]
    fn finding_schema_accepts_valid_finding() {
        assert!(validate_message_schema("FINDING: test\nSEVERITY: HIGH", Some("finding")).is_ok());
    }

    #[test]
    fn finding_schema_rejects_missing_severity() {
        assert!(validate_message_schema("FINDING: test", Some("finding")).is_err());
    }

    #[test]
    fn finding_schema_accepts_fix_messages() {
        assert!(validate_message_schema("FIX1 APPLIED: something", Some("finding")).is_ok());
    }

    #[test]
    fn finding_schema_accepts_complete_messages() {
        assert!(validate_message_schema("COMPLETE: 3 fixes", Some("finding")).is_ok());
    }

    #[test]
    fn unknown_schema_rejected() {
        assert!(validate_message_schema("anything", Some("invalid")).is_err());
    }

    #[test]
    fn no_schema_always_passes() {
        assert!(validate_message_schema("anything", None).is_ok());
    }

    // -----------------------------------------------------------------------
    // auto_fit_schema tests
    // -----------------------------------------------------------------------

    #[test]
    fn auto_fit_schema_no_schema_returns_body_unchanged() {
        assert_eq!(auto_fit_schema("hello", None), "hello");
    }

    #[test]
    fn auto_fit_schema_status_returns_body_unchanged() {
        assert_eq!(
            auto_fit_schema("STATUS: online", Some("status")),
            "STATUS: online"
        );
    }

    #[test]
    fn auto_fit_schema_unknown_schema_returns_body_unchanged() {
        assert_eq!(auto_fit_schema("body", Some("unknown")), "body");
    }

    #[test]
    fn auto_fit_schema_finding_wraps_plain_body() {
        let result = auto_fit_schema("memory leak detected", Some("finding"));
        assert!(result.contains("FINDING:"), "must inject FINDING:");
        assert!(result.contains("SEVERITY:"), "must inject SEVERITY:");
        // Resulting body must pass validation.
        assert!(validate_message_schema(&result, Some("finding")).is_ok());
    }

    #[test]
    fn auto_fit_schema_finding_detects_critical_severity() {
        let result = auto_fit_schema("critical failure in core", Some("finding"));
        assert!(result.contains("SEVERITY: CRITICAL"));
    }

    #[test]
    fn auto_fit_schema_finding_detects_high_severity_security() {
        let result = auto_fit_schema("security vulnerability found", Some("finding"));
        assert!(result.contains("SEVERITY: HIGH"));
    }

    #[test]
    fn auto_fit_schema_finding_detects_medium_severity_warning() {
        let result = auto_fit_schema("warning: possible overflow", Some("finding"));
        assert!(result.contains("SEVERITY: MEDIUM"));
    }

    #[test]
    fn auto_fit_schema_finding_defaults_to_low_severity() {
        let result = auto_fit_schema("minor nit in formatting", Some("finding"));
        assert!(result.contains("SEVERITY: LOW"));
    }

    #[test]
    fn auto_fit_schema_finding_already_valid_passthrough() {
        let body = "FINDING: pre-formatted\nSEVERITY: HIGH";
        assert_eq!(auto_fit_schema(body, Some("finding")), body);
    }

    #[test]
    fn auto_fit_schema_finding_fix_passthrough() {
        let body = "FIX applied to issue";
        assert_eq!(auto_fit_schema(body, Some("finding")), body);
    }

    #[test]
    fn auto_fit_schema_benchmark_wraps_plain_body() {
        let result = auto_fit_schema("test run finished", Some("benchmark"));
        assert!(result.contains('='), "must inject key=value pair");
        assert!(validate_message_schema(&result, Some("benchmark")).is_ok());
    }

    #[test]
    fn auto_fit_schema_benchmark_already_valid_passthrough() {
        let body = "duration=5.2s msgs=100";
        assert_eq!(auto_fit_schema(body, Some("benchmark")), body);
    }

    // -----------------------------------------------------------------------
    // enforce_schema_for_transport tests (Task 3.2 / WS3)
    // -----------------------------------------------------------------------

    #[test]
    fn enforce_schema_mcp_infers_from_topic_findings() {
        // Topic inference takes priority over transport default.
        assert_eq!(
            enforce_schema_for_transport("mcp", None, "code-findings"),
            Some("finding")
        );
    }

    #[test]
    fn enforce_schema_mcp_defaults_to_status_for_unknown_topic() {
        assert_eq!(
            enforce_schema_for_transport("mcp", None, "unknown-topic"),
            Some("status")
        );
    }

    #[test]
    fn enforce_schema_http_infers_from_topic_benchmark() {
        assert_eq!(
            enforce_schema_for_transport("http", None, "benchmark"),
            Some("benchmark")
        );
    }

    #[test]
    fn enforce_schema_http_defaults_to_status_for_unknown_topic() {
        assert_eq!(
            enforce_schema_for_transport("http", None, "chat"),
            Some("status")
        );
    }

    #[test]
    fn enforce_schema_cli_returns_none_for_unknown_topic() {
        assert_eq!(enforce_schema_for_transport("cli", None, "chat"), None);
    }

    #[test]
    fn enforce_schema_cli_infers_from_known_topic() {
        assert_eq!(
            enforce_schema_for_transport("cli", None, "status"),
            Some("status")
        );
    }

    #[test]
    fn enforce_schema_explicit_preserved_all_transports() {
        for transport in &["mcp", "http", "cli"] {
            assert_eq!(
                enforce_schema_for_transport(transport, Some("finding"), "anything"),
                Some("finding"),
                "explicit schema must be honoured for transport={transport}"
            );
        }
    }

    #[test]
    fn enforce_schema_explicit_benchmark_preserved() {
        assert_eq!(
            enforce_schema_for_transport("mcp", Some("benchmark"), "status"),
            Some("benchmark")
        );
    }

    #[test]
    fn enforce_schema_unknown_explicit_falls_back_to_topic_inference() {
        // Unknown explicit schema + known topic → topic inference wins.
        assert_eq!(
            enforce_schema_for_transport("cli", Some("bogus"), "ownership"),
            Some("status")
        );
    }

    #[test]
    fn enforce_schema_unknown_explicit_mcp_defaults_to_status() {
        // Unknown explicit schema + unknown topic on MCP → status default.
        assert_eq!(
            enforce_schema_for_transport("mcp", Some("bogus"), "chat"),
            Some("status")
        );
    }
}
