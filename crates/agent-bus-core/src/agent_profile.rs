//! Agent profile abstraction for multi-agent sync workflows.
//!
//! Generalizes the Codex-specific bridge into an extensible profile system so
//! that sync workflows work identically for Codex, Claude, Gemini, or custom
//! agent integrations.
//!
//! Each profile encapsulates:
//! - Configuration discovery (config file location, parsing)
//! - Default agent identity on the bus
//! - Message formatting preferences
//!
//! The generic [`run_agent_sync`] function accepts any profile and executes
//! the standard discover-read-normalize pipeline.
//!
//! # Example
//!
//! ```no_run
//! use agent_bus_core::agent_profile::{AgentProfile, CodexProfile, run_agent_sync};
//! # use agent_bus_core::settings::Settings;
//! # let settings = Settings::default();
//! let profile = CodexProfile;
//! let (result, findings) = run_agent_sync(&profile, &settings, 100)
//!     .expect("sync failed");
//! println!("Read {} messages", result.messages_read);
//! ```

use std::path::PathBuf;

use crate::error::Result;

use crate::models::Message;

// ---------------------------------------------------------------------------
// Agent configuration (profile-discovered)
// ---------------------------------------------------------------------------

/// Configuration values discovered from an agent's config file.
///
/// This is the generic counterpart to the Codex-specific `CodexConfig`.
/// All fields are optional with sensible defaults because any given agent
/// may only populate a subset.
#[derive(Debug, Clone, Default)]
pub struct AgentConfig {
    /// Agent kind that produced this config (e.g. `"codex"`, `"claude"`, `"gemini"`).
    pub agent_kind: String,
    /// Model identifier the agent is configured to use.
    pub model: String,
    /// Operating mode (e.g. Codex `approval_mode`, Claude model tier).
    pub mode: String,
    /// Agent ID the agent uses on the bus.
    pub agent_id: String,
    /// Maximum tokens per request (if configured).
    pub max_tokens: Option<u32>,
    /// Whether the config file was actually found on disk.
    pub config_found: bool,
}

impl AgentConfig {
    /// Return the effective bus agent ID, falling back to the profile default
    /// if none was found in the config file.
    #[must_use]
    pub fn effective_agent_id(&self, default: &str) -> String {
        if self.agent_id.is_empty() {
            default.to_owned()
        } else {
            self.agent_id.clone()
        }
    }
}

// ---------------------------------------------------------------------------
// AgentProfile trait
// ---------------------------------------------------------------------------

/// Trait defining agent-specific behavior for sync workflows.
///
/// Implementations encapsulate how to discover an agent's configuration and
/// how to format bus messages for that agent's consumption.
pub trait AgentProfile {
    /// Human-readable name for this profile (e.g. `"Codex"`, `"Claude"`).
    fn name(&self) -> &str;

    /// Default agent ID on the bus when no config file overrides it.
    fn default_agent_id(&self) -> &str;

    /// Discover this agent's configuration from disk/environment.
    ///
    /// # Errors
    ///
    /// Returns an error only for unrecoverable failures (e.g. home directory
    /// missing). A missing config file should return a default `AgentConfig`
    /// with `config_found = false`.
    fn discover(&self) -> Result<AgentConfig>;

    /// Format a bus message for this agent's preferred consumption format.
    fn format_message(&self, msg: &Message) -> String;
}

// ---------------------------------------------------------------------------
// CodexProfile
// ---------------------------------------------------------------------------

/// Codex CLI agent profile.
///
/// Discovers configuration from `~/.codex/config.toml` and formats messages
/// as structured markdown with labeled sections (the format Codex models
/// parse most reliably).
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexProfile;

#[allow(clippy::unnecessary_literal_bound)] // trait requires &str for GenericProfile
impl AgentProfile for CodexProfile {
    fn name(&self) -> &str {
        "Codex"
    }

    fn default_agent_id(&self) -> &str {
        "codex"
    }

    fn discover(&self) -> Result<AgentConfig> {
        let path = agent_config_path(".codex", "config.toml")?;
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Ok(AgentConfig {
                agent_kind: "codex".to_owned(),
                ..AgentConfig::default()
            });
        };
        Ok(parse_codex_toml(&text))
    }

    fn format_message(&self, msg: &Message) -> String {
        format_markdown_finding(msg)
    }
}

// ---------------------------------------------------------------------------
// ClaudeProfile
// ---------------------------------------------------------------------------

/// Claude Code agent profile.
///
/// Discovers configuration from `~/.claude.json` (the `mcpServers` section)
/// and formats messages as concise markdown suitable for Claude's context
/// window.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClaudeProfile;

#[allow(clippy::unnecessary_literal_bound)] // trait requires &str for GenericProfile
impl AgentProfile for ClaudeProfile {
    fn name(&self) -> &str {
        "Claude"
    }

    fn default_agent_id(&self) -> &str {
        "claude"
    }

    fn discover(&self) -> Result<AgentConfig> {
        let path = agent_config_path("", ".claude.json")?;
        let config_found = path.exists();
        Ok(AgentConfig {
            agent_kind: "claude".to_owned(),
            config_found,
            ..AgentConfig::default()
        })
    }

    fn format_message(&self, msg: &Message) -> String {
        format_markdown_finding(msg)
    }
}

// ---------------------------------------------------------------------------
// GeminiProfile
// ---------------------------------------------------------------------------

/// Gemini CLI agent profile.
///
/// Discovers configuration from `~/.gemini/settings.json` and formats
/// messages as markdown.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeminiProfile;

#[allow(clippy::unnecessary_literal_bound)] // trait requires &str for GenericProfile
impl AgentProfile for GeminiProfile {
    fn name(&self) -> &str {
        "Gemini"
    }

    fn default_agent_id(&self) -> &str {
        "gemini"
    }

    fn discover(&self) -> Result<AgentConfig> {
        let path = agent_config_path(".gemini", "settings.json")?;
        let config_found = path.exists();
        Ok(AgentConfig {
            agent_kind: "gemini".to_owned(),
            config_found,
            ..AgentConfig::default()
        })
    }

    fn format_message(&self, msg: &Message) -> String {
        format_markdown_finding(msg)
    }
}

// ---------------------------------------------------------------------------
// GenericProfile
// ---------------------------------------------------------------------------

/// A generic agent profile constructed at runtime.
///
/// Useful for custom or third-party agents that don't have a dedicated profile
/// implementation. Config discovery always returns defaults; the agent ID is
/// supplied at construction time.
#[derive(Debug, Clone)]
pub struct GenericProfile {
    name: String,
    default_agent_id: String,
}

impl GenericProfile {
    /// Create a generic profile with the given name and default agent ID.
    #[must_use]
    pub fn new(name: impl Into<String>, default_agent_id: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            default_agent_id: default_agent_id.into(),
        }
    }
}

impl AgentProfile for GenericProfile {
    fn name(&self) -> &str {
        &self.name
    }

    fn default_agent_id(&self) -> &str {
        &self.default_agent_id
    }

    fn discover(&self) -> Result<AgentConfig> {
        Ok(AgentConfig {
            agent_kind: self.default_agent_id.clone(),
            ..AgentConfig::default()
        })
    }

    fn format_message(&self, msg: &Message) -> String {
        format_markdown_finding(msg)
    }
}

// ---------------------------------------------------------------------------
// Profile registry
// ---------------------------------------------------------------------------

/// Known agent profile identifiers.
///
/// This enum doubles as a convenient way to select a profile by name from
/// CLI arguments or configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProfileKind {
    /// Codex CLI (`OpenAI`).
    Codex,
    /// Claude Code (Anthropic).
    Claude,
    /// Gemini CLI (Google).
    Gemini,
}

impl ProfileKind {
    /// Create the corresponding [`AgentProfile`] implementation.
    #[must_use]
    pub fn into_profile(self) -> Box<dyn AgentProfile> {
        match self {
            Self::Codex => Box::new(CodexProfile),
            Self::Claude => Box::new(ClaudeProfile),
            Self::Gemini => Box::new(GeminiProfile),
        }
    }

    /// All known profile kinds.
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[Self::Codex, Self::Claude, Self::Gemini]
    }

    /// Parse a profile kind from a string (case-insensitive).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name.to_lowercase().as_str() {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }

    /// The string name for this profile kind.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Gemini => "gemini",
        }
    }
}

impl std::fmt::Display for ProfileKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Generic sync pipeline
// ---------------------------------------------------------------------------

/// Result of an agent sync operation (profile-agnostic).
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentSyncResult {
    /// Profile name that was used for the sync.
    pub profile_name: String,
    /// Number of bus messages read.
    pub messages_read: usize,
    /// Number of finding-type messages normalized.
    pub findings_normalized: usize,
    /// Agent ID used for the query.
    pub agent_id: String,
    /// Model reported from the agent's config (empty if unknown).
    pub model: String,
    /// Whether the agent's config file was found on disk.
    pub config_found: bool,
}

/// Run a sync pipeline for any agent profile.
///
/// 1. Discovers the agent's configuration via the profile.
/// 2. Reads recent messages addressed to or from the agent.
/// 3. Normalizes finding-type messages.
/// 4. Returns a summary and the normalized findings.
///
/// # Errors
///
/// Returns an error if discovery or the Redis query fails.
pub fn run_agent_sync(
    profile: &dyn AgentProfile,
    settings: &crate::settings::Settings,
    limit: usize,
) -> Result<(AgentSyncResult, Vec<NormalizedFinding>)> {
    let config = profile
        .discover()
        .map_err(|_| crate::error::AgentBusError::Internal(format!("failed to discover {} config", profile.name())))?;

    let agent_id = config.effective_agent_id(profile.default_agent_id());

    let messages = crate::redis_bus::bus_list_messages(
        settings,
        Some(&agent_id),
        None,
        crate::models::MAX_HISTORY_MINUTES,
        limit,
        true,
    )
    .map_err(|_| crate::error::AgentBusError::Internal(format!("failed to read messages for agent {agent_id}")))?;

    let findings = normalize_findings(&messages);

    let result = AgentSyncResult {
        profile_name: profile.name().to_owned(),
        messages_read: messages.len(),
        findings_normalized: findings.len(),
        agent_id,
        model: config.model,
        config_found: config.config_found,
    };

    Ok((result, findings))
}

// ---------------------------------------------------------------------------
// Shared helpers (used by profiles and the legacy codex_bridge)
// ---------------------------------------------------------------------------

/// Resolve a path relative to the user's home directory.
///
/// Prefers `USERPROFILE` on Windows, then `HOME`.
///
/// - `subdir`: subfolder name (e.g. `".codex"`), or `""` for home root.
/// - `filename`: config file name (e.g. `"config.toml"`).
///
/// # Errors
///
/// Returns an error if the home directory cannot be determined.
pub(crate) fn agent_config_path(subdir: &str, filename: &str) -> Result<PathBuf> {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| crate::error::AgentBusError::Internal("cannot determine home directory (neither USERPROFILE nor HOME set)".to_string()))?;
    let mut path = PathBuf::from(home);
    if !subdir.is_empty() {
        path.push(subdir);
    }
    path.push(filename);
    Ok(path)
}

/// Extract a SEVERITY level from a bus message body.
///
/// Scans for `SEVERITY: <LEVEL>` patterns (case-insensitive) and returns the
/// first match, or `"UNKNOWN"` if none is found.
pub(crate) fn extract_severity(body: &str) -> &'static str {
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

/// Format a bus message as structured markdown with labeled sections.
///
/// This is the common formatting used by Codex, Claude, and Gemini profiles.
/// Agents that prefer a different format should override
/// [`AgentProfile::format_message`] instead of calling this directly.
#[must_use]
pub fn format_markdown_finding(msg: &Message) -> String {
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
// Finding normalization (agent-agnostic, shared with codex_bridge)
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
/// starts with `"review"`) are included. Other messages are silently skipped.
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
// Codex-specific TOML parsing (used by CodexProfile::discover)
// ---------------------------------------------------------------------------

/// Parse a Codex config TOML into an [`AgentConfig`].
///
/// Minimal line-by-line parser for flat `key = "value"` pairs. Avoids pulling
/// in the full `toml` crate for four fields.
fn parse_codex_toml(text: &str) -> AgentConfig {
    let mut cfg = AgentConfig {
        agent_kind: "codex".to_owned(),
        config_found: true,
        ..AgentConfig::default()
    };
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = extract_string_value(line, "model") {
            cfg.model = value;
        } else if let Some(value) = extract_string_value(line, "approval_mode") {
            cfg.mode = value;
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
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // AgentConfig
    // -----------------------------------------------------------------------

    #[test]
    fn agent_config_effective_id_uses_default_when_empty() {
        let cfg = AgentConfig::default();
        assert_eq!(cfg.effective_agent_id("codex"), "codex");
    }

    #[test]
    fn agent_config_effective_id_uses_configured_value() {
        let cfg = AgentConfig {
            agent_id: "codex-pro".to_owned(),
            ..AgentConfig::default()
        };
        assert_eq!(cfg.effective_agent_id("codex"), "codex-pro");
    }

    // -----------------------------------------------------------------------
    // ProfileKind
    // -----------------------------------------------------------------------

    #[test]
    fn profile_kind_from_name_case_insensitive() {
        assert_eq!(ProfileKind::from_name("Codex"), Some(ProfileKind::Codex));
        assert_eq!(ProfileKind::from_name("CLAUDE"), Some(ProfileKind::Claude));
        assert_eq!(ProfileKind::from_name("gemini"), Some(ProfileKind::Gemini));
        assert!(ProfileKind::from_name("unknown").is_none());
    }

    #[test]
    fn profile_kind_as_str_roundtrips() {
        for kind in ProfileKind::all() {
            assert_eq!(
                ProfileKind::from_name(kind.as_str()),
                Some(*kind),
                "roundtrip failed for {kind:?}"
            );
        }
    }

    #[test]
    fn profile_kind_display() {
        assert_eq!(ProfileKind::Codex.to_string(), "codex");
        assert_eq!(ProfileKind::Claude.to_string(), "claude");
        assert_eq!(ProfileKind::Gemini.to_string(), "gemini");
    }

    #[test]
    fn profile_kind_into_profile_returns_correct_names() {
        assert_eq!(ProfileKind::Codex.into_profile().name(), "Codex");
        assert_eq!(ProfileKind::Claude.into_profile().name(), "Claude");
        assert_eq!(ProfileKind::Gemini.into_profile().name(), "Gemini");
    }

    #[test]
    fn profile_kind_all_contains_three() {
        assert_eq!(ProfileKind::all().len(), 3);
    }

    // -----------------------------------------------------------------------
    // CodexProfile
    // -----------------------------------------------------------------------

    #[test]
    fn codex_profile_defaults() {
        let p = CodexProfile;
        assert_eq!(p.name(), "Codex");
        assert_eq!(p.default_agent_id(), "codex");
    }

    #[test]
    fn codex_profile_discover_returns_agent_config() {
        // Discovery may or may not find a config file, but should not error
        // unless HOME is missing.
        let p = CodexProfile;
        let result = p.discover();
        // On CI or machines without Codex, this still succeeds with defaults.
        assert!(result.is_ok());
        let cfg = result.expect("discover should succeed");
        assert_eq!(cfg.agent_kind, "codex");
    }

    // -----------------------------------------------------------------------
    // ClaudeProfile
    // -----------------------------------------------------------------------

    #[test]
    fn claude_profile_defaults() {
        let p = ClaudeProfile;
        assert_eq!(p.name(), "Claude");
        assert_eq!(p.default_agent_id(), "claude");
    }

    #[test]
    fn claude_profile_discover_returns_agent_config() {
        let p = ClaudeProfile;
        let result = p.discover();
        assert!(result.is_ok());
        let cfg = result.expect("discover should succeed");
        assert_eq!(cfg.agent_kind, "claude");
    }

    // -----------------------------------------------------------------------
    // GeminiProfile
    // -----------------------------------------------------------------------

    #[test]
    fn gemini_profile_defaults() {
        let p = GeminiProfile;
        assert_eq!(p.name(), "Gemini");
        assert_eq!(p.default_agent_id(), "gemini");
    }

    #[test]
    fn gemini_profile_discover_returns_agent_config() {
        let p = GeminiProfile;
        let result = p.discover();
        assert!(result.is_ok());
        let cfg = result.expect("discover should succeed");
        assert_eq!(cfg.agent_kind, "gemini");
    }

    // -----------------------------------------------------------------------
    // GenericProfile
    // -----------------------------------------------------------------------

    #[test]
    fn generic_profile_custom_name_and_id() {
        let p = GenericProfile::new("CustomBot", "custom-bot");
        assert_eq!(p.name(), "CustomBot");
        assert_eq!(p.default_agent_id(), "custom-bot");
    }

    #[test]
    fn generic_profile_discover_always_succeeds() {
        let p = GenericProfile::new("Test", "test-agent");
        let cfg = p.discover().expect("generic discover never fails");
        assert!(!cfg.config_found);
        assert_eq!(cfg.agent_kind, "test-agent");
    }

    // -----------------------------------------------------------------------
    // extract_severity (shared helper)
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
    fn extract_severity_returns_unknown_when_absent() {
        assert_eq!(extract_severity("just a plain message"), "UNKNOWN");
    }

    // -----------------------------------------------------------------------
    // format_markdown_finding
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
    fn format_markdown_finding_includes_all_sections() {
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
        let formatted = format_markdown_finding(&msg);
        assert!(formatted.contains("## Finding from claude"));
        assert!(formatted.contains("**Topic:** rust-findings"));
        assert!(formatted.contains("**Severity:** HIGH"));
        assert!(formatted.contains("FINDING: memory leak"));
        assert!(formatted.contains("repo:my-project"));
    }

    #[test]
    fn format_markdown_finding_includes_thread_id() {
        let mut msg = make_message("t1", "status", "done");
        msg.thread_id = Some("thread-abc".to_owned());
        let formatted = format_markdown_finding(&msg);
        assert!(formatted.contains("**Thread:** thread-abc"));

        msg.thread_id = None;
        let formatted_no_thread = format_markdown_finding(&msg);
        assert!(!formatted_no_thread.contains("**Thread:**"));
    }

    // -----------------------------------------------------------------------
    // normalize_findings
    // -----------------------------------------------------------------------

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
    // AgentSyncResult serialization
    // -----------------------------------------------------------------------

    #[test]
    fn agent_sync_result_serializes_to_json() {
        let result = AgentSyncResult {
            profile_name: "Codex".to_owned(),
            messages_read: 5,
            findings_normalized: 2,
            agent_id: "codex".to_owned(),
            model: "o3".to_owned(),
            config_found: true,
        };
        let json = serde_json::to_string(&result).expect("serialize");
        assert!(json.contains("\"messages_read\":5"));
        assert!(json.contains("\"model\":\"o3\""));
        assert!(json.contains("\"profile_name\":\"Codex\""));
    }

    // -----------------------------------------------------------------------
    // parse_codex_toml
    // -----------------------------------------------------------------------

    #[test]
    fn parse_codex_toml_extracts_known_fields() {
        let toml = r#"
model = "o3"
approval_mode = "full-auto"
agent_id = "codex"
max_tokens = 8192
"#;
        let cfg = parse_codex_toml(toml);
        assert_eq!(cfg.model, "o3");
        assert_eq!(cfg.mode, "full-auto");
        assert_eq!(cfg.agent_id, "codex");
        assert_eq!(cfg.max_tokens, Some(8192));
        assert!(cfg.config_found);
        assert_eq!(cfg.agent_kind, "codex");
    }

    #[test]
    fn parse_codex_toml_returns_defaults_for_empty_input() {
        let cfg = parse_codex_toml("");
        assert!(cfg.model.is_empty());
        assert!(cfg.mode.is_empty());
        assert!(cfg.agent_id.is_empty());
        assert!(cfg.max_tokens.is_none());
        assert!(cfg.config_found); // parse_codex_toml is called only when file exists
    }

    #[test]
    fn parse_codex_toml_ignores_unknown_keys() {
        let toml = r#"
model = "o3"
some_future_key = "value"
"#;
        let cfg = parse_codex_toml(toml);
        assert_eq!(cfg.model, "o3");
    }

    // -----------------------------------------------------------------------
    // extract_string_value / extract_u32_value
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
    // Profile format_message dispatches to format_markdown_finding
    // -----------------------------------------------------------------------

    #[test]
    fn all_builtin_profiles_format_consistently() {
        let msg = make_message("1", "rust-findings", "FINDING: X\nSEVERITY: HIGH");
        let expected = format_markdown_finding(&msg);

        let profiles: Vec<Box<dyn AgentProfile>> = vec![
            Box::new(CodexProfile),
            Box::new(ClaudeProfile),
            Box::new(GeminiProfile),
            Box::new(GenericProfile::new("Custom", "custom")),
        ];

        for profile in &profiles {
            assert_eq!(
                profile.format_message(&msg),
                expected,
                "format mismatch for profile {}",
                profile.name()
            );
        }
    }
}
