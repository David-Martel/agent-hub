//! Codex CLI integration bridge.
//!
//! Reads Codex config from `~/.codex/config.toml`, formats messages for Codex
//! consumption, and provides bidirectional finding sync between Claude and
//! Codex sessions via the agent bus.
//!
//! # Example
//!
//! ```no_run
//! let config = discover_codex().expect("Codex config not found");
//! println!("Codex model: {}", config.model);
//! ```

use std::path::PathBuf;

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::models::Message;

// ---------------------------------------------------------------------------
// Codex config types
// ---------------------------------------------------------------------------

/// Parsed representation of `~/.codex/config.toml`.
///
/// All fields are optional with sensible defaults because Codex only requires
/// the fields the user actually sets.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct CodexConfig {
    /// Codex model identifier (e.g. `"o3"`, `"gpt-4o"`).
    pub model: String,
    /// Approval mode: `"suggest"`, `"auto-edit"`, or `"full-auto"`.
    pub approval_mode: String,
    /// Agent ID Codex uses on the bus (convention: `"codex"`).
    pub agent_id: String,
    /// Maximum number of tokens per request.
    pub max_tokens: Option<u32>,
}

impl CodexConfig {
    /// Return the effective Codex bus agent ID, defaulting to `"codex"`.
    #[must_use]
    pub fn effective_agent_id(&self) -> &str {
        if self.agent_id.is_empty() {
            "codex"
        } else {
            &self.agent_id
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------

/// Resolve the path to `~/.codex/config.toml`.
///
/// Prefers `USERPROFILE` on Windows, then `HOME`.
fn codex_config_path() -> Result<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .context("cannot determine home directory (neither USERPROFILE nor HOME set)")?;
    Ok(PathBuf::from(home).join(".codex").join("config.toml"))
}

/// Discover Codex configuration and capabilities.
///
/// Reads `~/.codex/config.toml` and returns a parsed [`CodexConfig`].
/// If the file is absent or partially invalid, missing fields fall back to
/// their defaults so the bridge can still operate.
///
/// # Errors
///
/// Returns an error only if the home directory cannot be determined.
///
/// # Examples
///
/// ```no_run
/// let config = discover_codex().expect("home dir missing");
/// assert!(!config.effective_agent_id().is_empty());
/// ```
pub fn discover_codex() -> Result<CodexConfig> {
    let path = codex_config_path()?;
    let Ok(text) = std::fs::read_to_string(&path) else {
        // Config file absent — return defaults so the bridge can still run.
        return Ok(CodexConfig::default());
    };

    // toml crate is not in dependencies; parse the subset we care about by
    // looking for `key = "value"` lines.  This keeps the dep tree clean while
    // covering the fields we actually use.
    Ok(parse_codex_toml_simple(&text))
}

/// Minimal TOML parser for flat `key = "value"` pairs.
///
/// Codex config is a simple flat TOML file.  Rather than pulling in the full
/// `toml` crate, we extract only the four fields we care about using a
/// line-by-line scan.  This is intentionally narrow — if Codex adds nested
/// tables we can add the `toml` dep then.
fn parse_codex_toml_simple(text: &str) -> CodexConfig {
    let mut cfg = CodexConfig::default();
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = extract_string_value(line, "model") {
            cfg.model = value;
        } else if let Some(value) = extract_string_value(line, "approval_mode") {
            cfg.approval_mode = value;
        } else if let Some(value) = extract_string_value(line, "agent_id") {
            cfg.agent_id = value;
        } else if let Some(value) = extract_u32_value(line, "max_tokens") {
            cfg.max_tokens = Some(value);
        }
    }
    cfg
}

/// Extract `"value"` from a line like `key = "value"`.
fn extract_string_value(line: &str, key: &str) -> Option<String> {
    let prefix = format!("{key} =");
    let line = line.strip_prefix(&prefix)?.trim();
    // Handle both `"value"` and `'value'`
    ((line.starts_with('"') && line.ends_with('"'))
        || (line.starts_with('\'') && line.ends_with('\'')))
    .then(|| line[1..line.len() - 1].to_owned())
}

/// Extract an integer value from a line like `key = 1234`.
fn extract_u32_value(line: &str, key: &str) -> Option<u32> {
    let prefix = format!("{key} =");
    let line = line.strip_prefix(&prefix)?.trim();
    line.parse().ok()
}

// ---------------------------------------------------------------------------
// Message formatting
// ---------------------------------------------------------------------------

/// Extract a SEVERITY level from a bus message body.
///
/// Scans for `SEVERITY: <LEVEL>` patterns (case-insensitive) and returns the
/// first match, or `"UNKNOWN"` if none is found.
fn extract_severity(body: &str) -> &'static str {
    for line in body.lines() {
        let upper = line.to_uppercase();
        if upper.contains("SEVERITY:") {
            if upper.contains("CRITICAL") {
                return "CRITICAL";
            } else if upper.contains("HIGH") {
                return "HIGH";
            } else if upper.contains("MEDIUM") {
                return "MEDIUM";
            } else if upper.contains("LOW") {
                return "LOW";
            }
        }
    }
    "UNKNOWN"
}

/// Format a bus message for Codex CLI consumption.
///
/// Codex prefers structured markdown with clear labeled sections so the
/// model can reliably extract structured data without additional parsing.
///
/// # Examples
///
/// ```
/// use agent_bus_core::models::Message;
/// // format_for_codex produces a markdown document with labeled sections.
/// ```
// Call site exists in cmd_codex_sync via codex formatting pipeline.
#[allow(dead_code)]
#[must_use]
pub fn format_for_codex(msg: &Message) -> String {
    let severity = extract_severity(&msg.body);
    let tags = msg.tags.join(", ");
    let thread = msg
        .thread_id
        .as_deref()
        .map(|t| format!("\n**Thread:** {t}"))
        .unwrap_or_default();

    format!(
        "## Finding from {from}\n\
         **Topic:** {topic}\n\
         **Severity:** {severity}\n\
         **Priority:** {priority}{thread}\n\n\
         {body}\n\n\
         ---\n\
         Tags: {tags}",
        from = msg.from,
        topic = msg.topic,
        priority = msg.priority,
        body = msg.body,
    )
}

// ---------------------------------------------------------------------------
// Finding normalization
// ---------------------------------------------------------------------------

/// A normalized finding extracted from a raw bus message.
#[derive(Debug, Clone)]
pub struct NormalizedFinding {
    /// Original message ID.
    pub message_id: String,
    /// Sender agent.
    pub from_agent: String,
    /// Topic the finding was posted under.
    pub topic: String,
    /// Full message body.
    pub body: String,
    /// Extracted severity level.
    pub severity: String,
    /// Message tags.
    pub tags: Vec<String>,
    /// UTC timestamp.
    pub timestamp: String,
}

/// Normalize a list of raw bus messages into [`NormalizedFinding`] records.
///
/// Only messages whose topic suggests a finding (contains `"finding"` or
/// starts with `"review"`) are included.  Other messages are silently skipped.
///
/// # Examples
///
/// ```
/// # use agent_bus_core::models::Message;
/// # use agent_bus_core::codex_bridge::normalize_findings;
/// // normalize_findings(&messages) returns only finding-type messages.
/// ```
#[must_use]
pub fn normalize_findings(messages: &[Message]) -> Vec<NormalizedFinding> {
    messages
        .iter()
        .filter(|m| {
            let t = m.topic.as_str();
            t.contains("finding") || t.starts_with("review")
        })
        .map(|m| NormalizedFinding {
            message_id: m.id.clone(),
            from_agent: m.from.clone(),
            topic: m.topic.clone(),
            body: m.body.clone(),
            severity: extract_severity(&m.body).to_owned(),
            tags: m.tags.to_vec(),
            timestamp: m.timestamp_utc.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Sync summary
// ---------------------------------------------------------------------------

/// Result of a bidirectional Codex sync operation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CodexSyncResult {
    /// Number of bus messages read for Codex.
    pub messages_read: usize,
    /// Number of finding-type messages normalized.
    pub findings_normalized: usize,
    /// Agent ID of the discovered Codex instance.
    pub codex_agent_id: String,
    /// Codex model reported from config.
    pub codex_model: String,
    /// Whether the Codex config file was found.
    pub config_found: bool,
}

/// Run a bidirectional sync between Claude and Codex sessions.
///
/// 1. Discovers Codex configuration from `~/.codex/config.toml`.
/// 2. Reads recent messages addressed to or from the Codex agent.
/// 3. Normalizes finding-type messages.
/// 4. Returns a summary suitable for display or JSON output.
///
/// This function does not post any messages itself — callers are responsible
/// for routing the formatted findings to Codex via the bus.
///
/// # Errors
///
/// Returns an error if the home directory is unavailable or the Redis query
/// fails.
pub fn run_codex_sync(
    settings: &crate::settings::Settings,
    limit: usize,
) -> Result<(CodexSyncResult, Vec<NormalizedFinding>)> {
    let codex_cfg = discover_codex().context("failed to read Codex config")?;
    let config_found = !codex_cfg.model.is_empty() || !codex_cfg.approval_mode.is_empty();
    let agent_id = codex_cfg.effective_agent_id().to_owned();

    let messages = crate::redis_bus::bus_list_messages(
        settings,
        Some(&agent_id),
        None,
        crate::models::MAX_HISTORY_MINUTES,
        limit,
        true,
    )
    .context("failed to read messages from bus")?;

    let findings = normalize_findings(&messages);

    let result = CodexSyncResult {
        messages_read: messages.len(),
        findings_normalized: findings.len(),
        codex_agent_id: agent_id,
        codex_model: codex_cfg.model,
        config_found,
    };

    Ok((result, findings))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // extract_string_value
    // -----------------------------------------------------------------------

    #[test]
    fn extract_string_value_parses_double_quoted() {
        let result = extract_string_value(r#"model = "o3""#, "model");
        assert_eq!(result.as_deref(), Some("o3"));
    }

    #[test]
    fn extract_string_value_parses_single_quoted() {
        let result = extract_string_value("model = 'gpt-4o'", "model");
        assert_eq!(result.as_deref(), Some("gpt-4o"));
    }

    #[test]
    fn extract_string_value_returns_none_for_wrong_key() {
        let result = extract_string_value(r#"model = "o3""#, "approval_mode");
        assert!(result.is_none());
    }

    #[test]
    fn extract_string_value_returns_none_for_unquoted_value() {
        // Bare values (e.g. integers) should not match the string extractor.
        let result = extract_string_value("max_tokens = 4096", "max_tokens");
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // extract_u32_value
    // -----------------------------------------------------------------------

    #[test]
    fn extract_u32_value_parses_integer() {
        let result = extract_u32_value("max_tokens = 4096", "max_tokens");
        assert_eq!(result, Some(4096));
    }

    #[test]
    fn extract_u32_value_returns_none_for_string_value() {
        let result = extract_u32_value(r#"model = "o3""#, "model");
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // parse_codex_toml_simple
    // -----------------------------------------------------------------------

    #[test]
    fn parse_codex_toml_extracts_known_fields() {
        let toml = r#"
model = "o3"
approval_mode = "full-auto"
agent_id = "codex"
max_tokens = 8192
"#;
        let cfg = parse_codex_toml_simple(toml);
        assert_eq!(cfg.model, "o3");
        assert_eq!(cfg.approval_mode, "full-auto");
        assert_eq!(cfg.agent_id, "codex");
        assert_eq!(cfg.max_tokens, Some(8192));
    }

    #[test]
    fn parse_codex_toml_returns_defaults_for_empty_input() {
        let cfg = parse_codex_toml_simple("");
        assert!(cfg.model.is_empty());
        assert!(cfg.approval_mode.is_empty());
        assert!(cfg.agent_id.is_empty());
        assert!(cfg.max_tokens.is_none());
    }

    #[test]
    fn parse_codex_toml_ignores_unknown_keys() {
        let toml = r#"
model = "o3"
some_future_key = "value"
"#;
        let cfg = parse_codex_toml_simple(toml);
        assert_eq!(cfg.model, "o3");
        // Should not panic or error on unknown keys.
    }

    // -----------------------------------------------------------------------
    // CodexConfig::effective_agent_id
    // -----------------------------------------------------------------------

    #[test]
    fn effective_agent_id_returns_codex_when_empty() {
        let cfg = CodexConfig::default();
        assert_eq!(cfg.effective_agent_id(), "codex");
    }

    #[test]
    fn effective_agent_id_returns_configured_value() {
        let cfg = CodexConfig {
            agent_id: "codex-pro".to_owned(),
            ..CodexConfig::default()
        };
        assert_eq!(cfg.effective_agent_id(), "codex-pro");
    }

    // -----------------------------------------------------------------------
    // extract_severity
    // -----------------------------------------------------------------------

    #[test]
    fn extract_severity_finds_critical() {
        assert_eq!(
            extract_severity("FINDING: issue\nSEVERITY: CRITICAL\nbody"),
            "CRITICAL"
        );
    }

    #[test]
    fn extract_severity_finds_high() {
        assert_eq!(extract_severity("SEVERITY: HIGH\ndetails"), "HIGH");
    }

    #[test]
    fn extract_severity_prefers_critical_over_high_on_same_line() {
        // Critical check comes first in the chain, so CRITICAL wins.
        assert_eq!(extract_severity("SEVERITY: CRITICAL HIGH\n"), "CRITICAL");
    }

    #[test]
    fn extract_severity_returns_unknown_when_absent() {
        assert_eq!(extract_severity("just a plain message"), "UNKNOWN");
    }

    // -----------------------------------------------------------------------
    // format_for_codex
    // -----------------------------------------------------------------------

    #[test]
    fn format_for_codex_includes_all_sections() {
        let msg = Message {
            id: "test-id".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "claude".to_owned(),
            to: "codex".to_owned(),
            topic: "rust-findings".to_owned(),
            body: "FINDING: memory leak\nSEVERITY: HIGH\ndetails here".to_owned(),
            thread_id: None,
            tags: vec!["repo:my-project".to_owned()].into(),
            priority: "high".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        };
        let formatted = format_for_codex(&msg);
        assert!(formatted.contains("## Finding from claude"));
        assert!(formatted.contains("**Topic:** rust-findings"));
        assert!(formatted.contains("**Severity:** HIGH"));
        assert!(formatted.contains("FINDING: memory leak"));
        assert!(formatted.contains("repo:my-project"));
    }

    #[test]
    fn format_for_codex_includes_thread_id_when_set() {
        let mut msg = Message {
            id: "t1".to_owned(),
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "codex".to_owned(),
            to: "claude".to_owned(),
            topic: "status".to_owned(),
            body: "done".to_owned(),
            thread_id: Some("thread-abc".to_owned()),
            tags: vec![].into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        };
        let formatted = format_for_codex(&msg);
        assert!(formatted.contains("**Thread:** thread-abc"));

        msg.thread_id = None;
        let formatted_no_thread = format_for_codex(&msg);
        assert!(!formatted_no_thread.contains("**Thread:**"));
    }

    // -----------------------------------------------------------------------
    // normalize_findings
    // -----------------------------------------------------------------------

    fn make_message(id: &str, topic: &str, body: &str) -> Message {
        Message {
            id: id.to_owned(),
            timestamp_utc: "2026-01-01T00:00:00Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "claude".to_owned(),
            to: "codex".to_owned(),
            topic: topic.to_owned(),
            body: body.to_owned(),
            thread_id: None,
            tags: vec![].into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
        }
    }

    #[test]
    fn normalize_findings_includes_finding_topics() {
        let msgs = vec![
            make_message("1", "rust-findings", "FINDING: X\nSEVERITY: HIGH"),
            make_message("2", "review-pass", "FINDING: Y\nSEVERITY: LOW"),
        ];
        let findings = normalize_findings(&msgs);
        assert_eq!(findings.len(), 2);
    }

    #[test]
    fn normalize_findings_excludes_non_finding_topics() {
        let msgs = vec![
            make_message("1", "status", "all good"),
            make_message("2", "ownership", "claiming file"),
            make_message("3", "rust-findings", "FINDING: Z\nSEVERITY: MEDIUM"),
        ];
        let findings = normalize_findings(&msgs);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].message_id, "3");
    }

    #[test]
    fn normalize_findings_sets_severity_correctly() {
        let msgs = vec![make_message("1", "findings", "SEVERITY: CRITICAL\nfoo")];
        let findings = normalize_findings(&msgs);
        assert_eq!(findings[0].severity, "CRITICAL");
    }

    #[test]
    fn normalize_findings_returns_empty_for_no_matches() {
        let msgs = vec![make_message("1", "status", "running")];
        let findings = normalize_findings(&msgs);
        assert!(findings.is_empty());
    }

    // -----------------------------------------------------------------------
    // CodexSyncResult serialization
    // -----------------------------------------------------------------------

    #[test]
    fn codex_sync_result_serializes_to_json() {
        let result = CodexSyncResult {
            messages_read: 5,
            findings_normalized: 2,
            codex_agent_id: "codex".to_owned(),
            codex_model: "o3".to_owned(),
            config_found: true,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(json.contains("\"messages_read\":5"));
        assert!(json.contains("\"codex_model\":\"o3\""));
    }
}
