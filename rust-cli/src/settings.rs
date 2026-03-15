//! Environment-variable configuration, read once at startup.

use anyhow::{Result, bail};

/// All environment-variable configuration, read at startup.
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
    pub(crate) fn from_env() -> Self {
        Self {
            redis_url: env_or("AGENT_BUS_REDIS_URL", "redis://localhost:6380/0"),
            database_url: env_or_optional_default(
                "AGENT_BUS_DATABASE_URL",
                "postgresql://postgres@localhost:5432/redis_backend",
            ),
            stream_key: env_or("AGENT_BUS_STREAM_KEY", "agent_bus:messages"),
            channel_key: env_or("AGENT_BUS_CHANNEL", "agent_bus:events"),
            presence_prefix: env_or("AGENT_BUS_PRESENCE_PREFIX", "agent_bus:presence:"),
            message_table: env_or("AGENT_BUS_MESSAGE_TABLE", "agent_bus.messages"),
            presence_event_table: env_or(
                "AGENT_BUS_PRESENCE_EVENT_TABLE",
                "agent_bus.presence_events",
            ),
            stream_maxlen: env_parse("AGENT_BUS_STREAM_MAXLEN", 5000),
            service_agent_id: env_or("AGENT_BUS_SERVICE_AGENT_ID", "agent-bus"),
            startup_enabled: env_or("AGENT_BUS_STARTUP_ENABLED", "true") != "false",
            startup_recipient: env_or("AGENT_BUS_STARTUP_RECIPIENT", "all"),
            startup_topic: env_or("AGENT_BUS_STARTUP_TOPIC", "status"),
            startup_body: env_or("AGENT_BUS_STARTUP_BODY", "agent-bus is up and running"),
            server_host: env_or("AGENT_BUS_SERVER_HOST", "localhost"),
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

pub(crate) fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_owned())
}

pub(crate) fn env_or_optional_default(key: &str, default: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        Err(_) => Some(default.to_owned()),
    }
}

pub(crate) fn env_parse<T: std::str::FromStr>(key: &str, default: T) -> T {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
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

    #[test]
    fn env_or_returns_default_for_missing_var() {
        let result = env_or("__AGENT_BUS_TEST_NONEXISTENT_KEY__", "fallback");
        assert_eq!(result, "fallback");
    }

    #[test]
    fn env_parse_returns_default_for_missing_var() {
        let result: u64 = env_parse("__AGENT_BUS_TEST_NONEXISTENT_KEY__", 42);
        assert_eq!(result, 42);
    }

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
    // Settings::validate — localhost enforcement (Task 1)
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
    // Settings::validate — identifier checks (Task 2)
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
}
