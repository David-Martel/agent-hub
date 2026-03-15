//! Environment-variable configuration, read once at startup.

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
}
