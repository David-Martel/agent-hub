//! Input validation helpers.

use anyhow::{Context as _, Result, anyhow, bail};

pub(crate) const VALID_PRIORITIES: &[&str] = &["low", "normal", "high", "urgent"];

pub(crate) fn validate_priority(p: &str) -> Result<()> {
    if VALID_PRIORITIES.contains(&p) {
        Ok(())
    } else {
        Err(anyhow!(
            "invalid priority '{p}'; must be one of: {}",
            VALID_PRIORITIES.join(", ")
        ))
    }
}

pub(crate) fn non_empty<'a>(val: &'a str, name: &str) -> Result<&'a str> {
    let trimmed = val.trim();
    if trimmed.is_empty() {
        Err(anyhow!("{name} must not be empty"))
    } else {
        Ok(trimmed)
    }
}

pub(crate) fn parse_metadata_arg(metadata: Option<&str>) -> Result<serde_json::Value> {
    match metadata {
        None => Ok(serde_json::Value::Object(serde_json::Map::new())),
        Some(s) => serde_json::from_str(s).context("--metadata must be valid JSON object"),
    }
}

/// Known schema identifiers for structured message bodies.
pub(crate) const SCHEMA_FINDING: &str = "finding";
pub(crate) const SCHEMA_STATUS: &str = "status";
pub(crate) const SCHEMA_BENCHMARK: &str = "benchmark";

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
pub(crate) fn validate_message_schema(body: &str, schema: Option<&str>) -> Result<()> {
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
                bail!("Schema 'finding' requires FINDING:, FIX, TAGGED:, or COMPLETE in body");
            }
            // When a FINDING: is declared, SEVERITY: must also be present.
            if body.contains("FINDING:") && !body.contains("SEVERITY:") {
                bail!("Schema 'finding' requires SEVERITY: when FINDING: is present");
            }
            Ok(())
        }
        SCHEMA_STATUS => {
            // Must contain a STATUS: line.
            if !body.contains("STATUS:") {
                bail!("Schema 'status' requires STATUS: in body");
            }
            Ok(())
        }
        SCHEMA_BENCHMARK => {
            // Must contain measurable metrics.
            if !body.contains("agents=") && !body.contains("msgs=") && !body.contains("duration=") {
                bail!("Schema 'benchmark' requires metrics (agents=, msgs=, duration=) in body");
            }
            Ok(())
        }
        other => {
            bail!(
                "Unknown message schema: '{other}'. Valid: {SCHEMA_FINDING}, {SCHEMA_STATUS}, {SCHEMA_BENCHMARK}"
            );
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
    fn validate_message_schema_status_rejects_missing_keyword() {
        assert!(validate_message_schema("all good", Some("status")).is_err());
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
}
