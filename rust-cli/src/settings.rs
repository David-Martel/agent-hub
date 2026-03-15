//! Environment-variable and config-file configuration, read once at startup.
//!
//! Resolution order (highest wins):
//! 1. Environment variables
//! 2. Config file (`AGENT_BUS_CONFIG` env var path, or `~/.config/agent-bus/config.json`)
//! 3. Hardcoded defaults

use anyhow::{Result, bail};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Config file
// ---------------------------------------------------------------------------

/// Mirrors every [`Settings`] field as an `Option<T>` so absent JSON keys fall
/// through to the hardcoded defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct ConfigFile {
    pub(crate) redis_url: Option<String>,
    pub(crate) database_url: Option<String>,
    pub(crate) stream_key: Option<String>,
    pub(crate) channel: Option<String>,
    pub(crate) presence_prefix: Option<String>,
    pub(crate) message_table: Option<String>,
    pub(crate) presence_event_table: Option<String>,
    pub(crate) stream_maxlen: Option<u64>,
    pub(crate) server_host: Option<String>,
    pub(crate) service_agent_id: Option<String>,
    pub(crate) startup_enabled: Option<bool>,
    pub(crate) startup_recipient: Option<String>,
    pub(crate) startup_topic: Option<String>,
    pub(crate) startup_body: Option<String>,
}

/// Resolve the path for the config file.
///
/// Checks `AGENT_BUS_CONFIG` first, then falls back to
/// `%USERPROFILE%\.config\agent-bus\config.json` (Windows) or
/// `~/.config/agent-bus/config.json` (other platforms).
fn config_file_path() -> Option<std::path::PathBuf> {
    if let Ok(custom) = std::env::var("AGENT_BUS_CONFIG") {
        if !custom.trim().is_empty() {
            return Some(std::path::PathBuf::from(custom));
        }
    }

    // Prefer USERPROFILE on Windows; fall back to HOME on Unix.
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()?;
    let mut path = std::path::PathBuf::from(home);
    path.push(".config");
    path.push("agent-bus");
    path.push("config.json");
    Some(path)
}

/// Load the config file, returning [`ConfigFile::default()`] on any error.
pub(crate) fn load_config_file() -> ConfigFile {
    let Some(path) = config_file_path() else {
        return ConfigFile::default();
    };

    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str::<ConfigFile>(&text) {
            Ok(cfg) => {
                tracing::debug!("loaded agent-bus config from {}", path.display());
                cfg
            }
            Err(err) => {
                tracing::debug!(
                    "agent-bus config at {} could not be parsed ({}); using defaults",
                    path.display(),
                    err
                );
                ConfigFile::default()
            }
        },
        Err(_) => ConfigFile::default(),
    }
}

/// Write a default config file if one does not already exist.
fn maybe_write_default_config(path: &std::path::Path) {
    if path.exists() {
        return;
    }
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let default_json = r#"{
  "redis_url": "redis://localhost:6380/0",
  "database_url": "postgresql://postgres@localhost:5300/redis_backend",
  "stream_key": "agent_bus:messages",
  "channel": "agent_bus:events",
  "presence_prefix": "agent_bus:presence:",
  "message_table": "agent_bus.messages",
  "presence_event_table": "agent_bus.presence_events",
  "stream_maxlen": 100000,
  "server_host": "localhost",
  "service_agent_id": "agent-bus",
  "startup_enabled": true,
  "startup_recipient": "all",
  "startup_topic": "status",
  "startup_body": "agent-bus is up and running"
}
"#;
    if std::fs::write(path, default_json).is_ok() {
        tracing::debug!("wrote default agent-bus config to {}", path.display());
    }
}

// ---------------------------------------------------------------------------
// 3-tier resolver helpers
// ---------------------------------------------------------------------------

/// Return the first non-`None` value among: env var → config file value → hardcoded default.
fn resolve(env_key: &str, config_value: Option<&str>, default: &str) -> String {
    std::env::var(env_key)
        .ok()
        .unwrap_or_else(|| config_value.map_or_else(|| default.to_owned(), str::to_owned))
}

/// Like [`resolve`] but parses `T` from a string; falls back to `default` on parse failure.
fn resolve_parse<T>(env_key: &str, config_value: Option<T>, default: T) -> T
where
    T: std::str::FromStr + Copy,
{
    if let Ok(raw) = std::env::var(env_key) {
        if let Ok(parsed) = raw.parse::<T>() {
            return parsed;
        }
    }
    config_value.unwrap_or(default)
}

/// Three-tier resolution for an optional string that has a non-`None` hardcoded default.
///
/// If the env var is set to an empty string, returns `None`.
fn resolve_optional_url(
    env_key: &str,
    config_value: Option<&str>,
    default: &str,
) -> Option<String> {
    match std::env::var(env_key) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        Err(_) => Some(config_value.map_or_else(|| default.to_owned(), str::to_owned)),
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// All configuration, read once at startup.
#[derive(Debug, Clone)]
pub(crate) struct Settings {
    pub(crate) redis_url: String,
    pub(crate) database_url: Option<String>,
    pub(crate) stream_key: String,
    pub(crate) channel_key: String,
    pub(crate) presence_prefix: String,
    pub(crate) message_table: String,
    pub(crate) presence_event_table: String,
    pub(crate) stream_maxlen: u64,
    pub(crate) service_agent_id: String,
    pub(crate) startup_enabled: bool,
    pub(crate) startup_recipient: String,
    pub(crate) startup_topic: String,
    pub(crate) startup_body: String,
    pub(crate) server_host: String,
}

impl Settings {
    /// Build [`Settings`] using the three-tier resolution order:
    /// env vars > config file > hardcoded defaults.
    pub(crate) fn from_env() -> Self {
        // Write a starter config if the file is absent (best-effort, silent on
        // failure so we never prevent the process from starting).
        if let Some(path) = config_file_path() {
            maybe_write_default_config(&path);
        }

        let cfg = load_config_file();

        let startup_enabled_str = resolve(
            "AGENT_BUS_STARTUP_ENABLED",
            cfg.startup_enabled
                .map(|b| if b { "true" } else { "false" }),
            "true",
        );

        Self {
            redis_url: resolve(
                "AGENT_BUS_REDIS_URL",
                cfg.redis_url.as_deref(),
                "redis://localhost:6380/0",
            ),
            database_url: resolve_optional_url(
                "AGENT_BUS_DATABASE_URL",
                cfg.database_url.as_deref(),
                "postgresql://postgres@localhost:5300/redis_backend",
            ),
            stream_key: resolve(
                "AGENT_BUS_STREAM_KEY",
                cfg.stream_key.as_deref(),
                "agent_bus:messages",
            ),
            channel_key: resolve(
                "AGENT_BUS_CHANNEL",
                cfg.channel.as_deref(),
                "agent_bus:events",
            ),
            presence_prefix: resolve(
                "AGENT_BUS_PRESENCE_PREFIX",
                cfg.presence_prefix.as_deref(),
                "agent_bus:presence:",
            ),
            message_table: resolve(
                "AGENT_BUS_MESSAGE_TABLE",
                cfg.message_table.as_deref(),
                "agent_bus.messages",
            ),
            presence_event_table: resolve(
                "AGENT_BUS_PRESENCE_EVENT_TABLE",
                cfg.presence_event_table.as_deref(),
                "agent_bus.presence_events",
            ),
            stream_maxlen: resolve_parse("AGENT_BUS_STREAM_MAXLEN", cfg.stream_maxlen, 100000),
            service_agent_id: resolve(
                "AGENT_BUS_SERVICE_AGENT_ID",
                cfg.service_agent_id.as_deref(),
                "agent-bus",
            ),
            startup_enabled: startup_enabled_str != "false",
            startup_recipient: resolve(
                "AGENT_BUS_STARTUP_RECIPIENT",
                cfg.startup_recipient.as_deref(),
                "all",
            ),
            startup_topic: resolve(
                "AGENT_BUS_STARTUP_TOPIC",
                cfg.startup_topic.as_deref(),
                "status",
            ),
            startup_body: resolve(
                "AGENT_BUS_STARTUP_BODY",
                cfg.startup_body.as_deref(),
                "agent-bus is up and running",
            ),
            server_host: resolve(
                "AGENT_BUS_SERVER_HOST",
                cfg.server_host.as_deref(),
                "localhost",
            ),
        }
    }

    /// Validate that all configured values are safe for a local-only agent bus.
    ///
    /// Enforces:
    /// - All URLs must resolve to localhost (localhost, 127.0.0.1, or `::1`).
    /// - Stream keys, channel keys, and table names must be non-empty and
    ///   free of whitespace characters.
    ///
    /// # Errors
    ///
    /// Returns an error describing the first validation failure found.
    ///
    /// # Examples
    ///
    /// ```
    /// let settings = Settings::from_env();
    /// settings.validate().expect("default settings should be valid");
    /// ```
    pub(crate) fn validate(&self) -> Result<()> {
        validate_localhost_url(&self.redis_url, "AGENT_BUS_REDIS_URL")?;
        if let Some(ref db_url) = self.database_url {
            validate_localhost_url(db_url, "AGENT_BUS_DATABASE_URL")?;
        }
        if !is_localhost(&self.server_host) {
            bail!(
                "AGENT_BUS_SERVER_HOST must be localhost or 127.0.0.1, got '{}'",
                self.server_host
            );
        }
        validate_identifier(&self.stream_key, "AGENT_BUS_STREAM_KEY")?;
        validate_identifier(&self.channel_key, "AGENT_BUS_CHANNEL")?;
        validate_identifier(&self.presence_prefix, "AGENT_BUS_PRESENCE_PREFIX")?;
        validate_identifier(&self.message_table, "AGENT_BUS_MESSAGE_TABLE")?;
        validate_identifier(&self.presence_event_table, "AGENT_BUS_PRESENCE_EVENT_TABLE")?;
        Ok(())
    }
}

fn is_localhost(host: &str) -> bool {
    let h = host.trim().to_lowercase();
    h == "localhost" || h == "127.0.0.1" || h == "::1"
}

/// Extract the host from a URL and verify it is localhost.
fn validate_localhost_url(url: &str, env_var: &str) -> Result<()> {
    let Some(scheme_end) = url.find("://") else {
        return Ok(()); // not a URL, skip
    };
    let after_scheme = &url[scheme_end + 3..];
    let authority = after_scheme.split('/').next().unwrap_or("");
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = host_port.split(':').next().unwrap_or("");
    if !is_localhost(host) {
        bail!("{env_var} must use localhost, got host '{host}' in '{url}'");
    }
    Ok(())
}

/// Verify an identifier is non-empty and contains no whitespace.
fn validate_identifier(value: &str, env_var: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{env_var} must not be empty");
    }
    if value.contains(' ') || value.contains('\n') || value.contains('\t') {
        bail!("{env_var} must not contain whitespace, got '{value}'");
    }
    Ok(())
}

pub(crate) fn redact_url(value: &str) -> String {
    let Some(scheme_end) = value.find("://") else {
        return value.to_owned();
    };
    let authority_start = scheme_end + 3;
    let authority_end = value[authority_start..]
        .find(['/', '?', '#'])
        .map_or(value.len(), |index| authority_start + index);
    let authority = &value[authority_start..authority_end];
    let Some(at_index) = authority.rfind('@') else {
        return value.to_owned();
    };

    let host_part = &authority[at_index + 1..];
    let redacted_authority = if authority[..at_index].contains(':') {
        format!("***:***@{host_part}")
    } else {
        format!("***@{host_part}")
    };
    format!(
        "{}{}{}",
        &value[..authority_start],
        redacted_authority,
        &value[authority_end..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // ConfigFile::default — all fields are None
    // -----------------------------------------------------------------------

    #[test]
    fn config_file_default_has_all_none_fields() {
        let cfg = ConfigFile::default();
        assert!(cfg.redis_url.is_none());
        assert!(cfg.database_url.is_none());
        assert!(cfg.stream_key.is_none());
        assert!(cfg.channel.is_none());
        assert!(cfg.presence_prefix.is_none());
        assert!(cfg.message_table.is_none());
        assert!(cfg.presence_event_table.is_none());
        assert!(cfg.stream_maxlen.is_none());
        assert!(cfg.server_host.is_none());
        assert!(cfg.service_agent_id.is_none());
        assert!(cfg.startup_enabled.is_none());
        assert!(cfg.startup_recipient.is_none());
        assert!(cfg.startup_topic.is_none());
        assert!(cfg.startup_body.is_none());
    }

    // -----------------------------------------------------------------------
    // resolve() — priority: env > config > default
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_prefers_env_over_config_and_default() {
        // Use a unique key unlikely to be set in the test environment.
        let key = "__AGENT_BUS_TEST_RESOLVE_ENV__";
        // SAFETY: single-threaded test; no other thread reads this var.
        unsafe { std::env::set_var(key, "from_env") };
        let result = resolve(key, Some("from_config"), "from_default");
        // SAFETY: paired remove after set above.
        unsafe { std::env::remove_var(key) };
        assert_eq!(result, "from_env");
    }

    #[test]
    fn resolve_falls_back_to_config_when_env_absent() {
        let key = "__AGENT_BUS_TEST_RESOLVE_CFG__";
        // SAFETY: single-threaded test; no other thread reads this var.
        unsafe { std::env::remove_var(key) };
        let result = resolve(key, Some("from_config"), "from_default");
        assert_eq!(result, "from_config");
    }

    #[test]
    fn resolve_falls_back_to_default_when_both_absent() {
        let key = "__AGENT_BUS_TEST_RESOLVE_DEF__";
        // SAFETY: single-threaded test; no other thread reads this var.
        unsafe { std::env::remove_var(key) };
        let result = resolve(key, None, "from_default");
        assert_eq!(result, "from_default");
    }

    // -----------------------------------------------------------------------
    // Existing tests (unchanged)
    // -----------------------------------------------------------------------

    #[test]
    fn settings_from_env_has_sane_defaults() {
        let s = Settings::from_env();
        assert!(s.redis_url.starts_with("redis://"));
        assert!(!s.stream_key.is_empty());
        assert!(!s.channel_key.is_empty());
        assert!(s.stream_maxlen > 0);
    }

    #[test]
    fn redact_url_hides_credentials() {
        assert_eq!(
            redact_url("postgresql://postgres:secret@localhost:5432/redis_backend"),
            "postgresql://***:***@localhost:5432/redis_backend"
        );
        assert_eq!(
            redact_url("redis://default@localhost:6380/0"),
            "redis://***@localhost:6380/0"
        );
    }

    #[test]
    fn redact_url_leaves_plain_urls_unchanged() {
        assert_eq!(
            redact_url("redis://localhost:6380/0"),
            "redis://localhost:6380/0"
        );
    }

    // -----------------------------------------------------------------------
    // Settings::validate — localhost enforcement
    // -----------------------------------------------------------------------

    #[test]
    fn validate_rejects_non_localhost_redis() {
        let mut s = Settings::from_env();
        s.redis_url = "redis://remote-host:6380/0".to_owned();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_non_localhost_database() {
        let mut s = Settings::from_env();
        s.database_url = Some("postgresql://postgres@remote:5432/db".to_owned());
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_non_localhost_server_host() {
        let mut s = Settings::from_env();
        s.server_host = "0.0.0.0".to_owned();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_accepts_localhost_variants() {
        let mut s = Settings::from_env();
        s.redis_url = "redis://localhost:6380/0".to_owned();
        s.database_url = Some("postgresql://postgres@localhost:5432/db".to_owned());
        s.server_host = "localhost".to_owned();
        assert!(s.validate().is_ok());
    }

    #[test]
    fn validate_accepts_127_0_0_1() {
        let mut s = Settings::from_env();
        s.redis_url = "redis://127.0.0.1:6380/0".to_owned();
        s.server_host = "127.0.0.1".to_owned();
        assert!(s.validate().is_ok());
    }

    #[test]
    fn validate_accepts_none_database() {
        let mut s = Settings::from_env();
        s.database_url = None;
        assert!(s.validate().is_ok());
    }

    // -----------------------------------------------------------------------
    // Settings::validate — identifier checks
    // -----------------------------------------------------------------------

    #[test]
    fn validate_rejects_empty_stream_key() {
        let mut s = Settings::from_env();
        s.stream_key = String::new();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_stream_key_with_spaces() {
        let mut s = Settings::from_env();
        s.stream_key = "bad stream key".to_owned();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_table_name() {
        let mut s = Settings::from_env();
        s.message_table = String::new();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_accepts_dotted_table_name() {
        let s = Settings::from_env();
        // default is "agent_bus.messages" which contains a dot — should pass
        assert!(s.validate().is_ok());
    }

    // -----------------------------------------------------------------------
    // Config file round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn config_file_deserializes_from_json() {
        let json = r#"{
            "redis_url": "redis://localhost:9999/1",
            "stream_maxlen": 1234
        }"#;
        let cfg: ConfigFile = serde_json::from_str(json).expect("valid JSON");
        assert_eq!(cfg.redis_url.as_deref(), Some("redis://localhost:9999/1"));
        assert_eq!(cfg.stream_maxlen, Some(1234));
        // Fields absent from JSON remain None.
        assert!(cfg.server_host.is_none());
    }

    #[test]
    fn config_file_partial_json_leaves_missing_fields_none() {
        let json = r#"{"startup_body": "hello"}"#;
        let cfg: ConfigFile = serde_json::from_str(json).expect("valid JSON");
        assert_eq!(cfg.startup_body.as_deref(), Some("hello"));
        assert!(cfg.redis_url.is_none());
        assert!(cfg.stream_maxlen.is_none());
    }
}
