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
