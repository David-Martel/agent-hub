//! Input validation helpers.

use anyhow::{Context as _, Result, anyhow};

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
}
