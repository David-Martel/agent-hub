//! Codex CLI integration bridge.
//!
//! Reads Codex config from `~/.codex/config.toml`, formats messages for Codex
//! consumption, and provides bidirectional finding sync between Claude and
//! Codex sessions via the agent bus.
//!
//! This module is now a backward-compatible facade over the generic
//! [`agent_profile`](crate::agent_profile) system.  All Codex-specific logic
//! delegates to [`CodexProfile`](crate::agent_profile::CodexProfile) while
//! preserving the existing public API so that the `codex-sync` CLI command
//! continues to work without changes.
//!
//! # Example
//!
//! ```no_run
//! let config = agent_bus_core::codex_bridge::discover_codex()
//!     .expect("Codex config not found");
//! println!("Codex model: {}", config.model);
//! ```

use anyhow::{Context as _, Result};
use serde::Deserialize;

use crate::agent_profile::{
    self, AgentProfile as _, CodexProfile, NormalizedFinding, format_markdown_finding,
};
use crate::models::Message;

// Re-export NormalizedFinding so callers that import from codex_bridge still work.
pub use crate::agent_profile::NormalizedFinding as Finding;

// ---------------------------------------------------------------------------
// Codex config types (backward-compatible)
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

/// Convert a generic [`AgentConfig`](agent_profile::AgentConfig) into the
/// legacy [`CodexConfig`] type.
impl From<agent_profile::AgentConfig> for CodexConfig {
    fn from(cfg: agent_profile::AgentConfig) -> Self {
        Self {
            model: cfg.model,
            approval_mode: cfg.mode,
            agent_id: cfg.agent_id,
            max_tokens: cfg.max_tokens,
        }
    }
}

// ---------------------------------------------------------------------------
// Discovery (delegates to CodexProfile)
// ---------------------------------------------------------------------------

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
/// let config = agent_bus_core::codex_bridge::discover_codex()
///     .expect("home dir missing");
/// assert!(!config.effective_agent_id().is_empty());
/// ```
pub fn discover_codex() -> Result<CodexConfig> {
    let profile = CodexProfile;
    let agent_config = profile.discover().context("failed to read Codex config")?;
    Ok(CodexConfig::from(agent_config))
}

// ---------------------------------------------------------------------------
// Message formatting (delegates to shared helper)
// ---------------------------------------------------------------------------

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
    format_markdown_finding(msg)
}

// ---------------------------------------------------------------------------
// Finding normalization (delegates to agent_profile)
// ---------------------------------------------------------------------------

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
    agent_profile::normalize_findings(messages)
}

// ---------------------------------------------------------------------------
// Sync summary (backward-compatible)
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

/// Convert from the generic [`AgentSyncResult`](agent_profile::AgentSyncResult)
/// into the Codex-specific legacy result type.
impl From<agent_profile::AgentSyncResult> for CodexSyncResult {
    fn from(r: agent_profile::AgentSyncResult) -> Self {
        Self {
            messages_read: r.messages_read,
            findings_normalized: r.findings_normalized,
            codex_agent_id: r.agent_id,
            codex_model: r.model,
            config_found: r.config_found,
        }
    }
}

/// Run a bidirectional sync between Claude and Codex sessions.
///
/// 1. Discovers Codex configuration from `~/.codex/config.toml`.
/// 2. Reads recent messages addressed to or from the Codex agent.
/// 3. Normalizes finding-type messages.
/// 4. Returns a summary suitable for display or JSON output.
///
/// This function does not post any messages itself -- callers are responsible
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
    let profile = CodexProfile;
    let (generic_result, findings) = agent_profile::run_agent_sync(&profile, settings, limit)?;
    Ok((CodexSyncResult::from(generic_result), findings))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
    // discover_codex
    // -----------------------------------------------------------------------

    #[test]
    fn discover_codex_returns_config() {
        // On CI or machines without Codex config, this returns defaults.
        let config = discover_codex().expect("discover should succeed");
        assert!(!config.effective_agent_id().is_empty());
    }

    use crate::agent_profile::extract_severity;

    // -----------------------------------------------------------------------
    // extract_severity (via shared helper)
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

    // -----------------------------------------------------------------------
    // Conversion roundtrip: AgentConfig -> CodexConfig
    // -----------------------------------------------------------------------

    #[test]
    fn agent_config_converts_to_codex_config() {
        let agent_cfg = agent_profile::AgentConfig {
            agent_kind: "codex".to_owned(),
            model: "o3".to_owned(),
            mode: "full-auto".to_owned(),
            agent_id: "codex-v2".to_owned(),
            max_tokens: Some(4096),
            config_found: true,
        };
        let codex_cfg = CodexConfig::from(agent_cfg);
        assert_eq!(codex_cfg.model, "o3");
        assert_eq!(codex_cfg.approval_mode, "full-auto");
        assert_eq!(codex_cfg.agent_id, "codex-v2");
        assert_eq!(codex_cfg.max_tokens, Some(4096));
    }

    // -----------------------------------------------------------------------
    // Conversion roundtrip: AgentSyncResult -> CodexSyncResult
    // -----------------------------------------------------------------------

    #[test]
    fn agent_sync_result_converts_to_codex_sync_result() {
        let generic = agent_profile::AgentSyncResult {
            profile_name: "Codex".to_owned(),
            messages_read: 10,
            findings_normalized: 3,
            agent_id: "codex".to_owned(),
            model: "gpt-4o".to_owned(),
            config_found: true,
        };
        let codex = CodexSyncResult::from(generic);
        assert_eq!(codex.messages_read, 10);
        assert_eq!(codex.findings_normalized, 3);
        assert_eq!(codex.codex_agent_id, "codex");
        assert_eq!(codex.codex_model, "gpt-4o");
        assert!(codex.config_found);
    }
}
