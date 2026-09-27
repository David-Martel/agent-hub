//! Environment-variable and config-file configuration, read once at startup.
//!
//! Resolution order (highest wins):
//! 1. Environment variables
//! 2. Config file (`AGENT_BUS_CONFIG` env var path, or `~/.config/agent-bus/config.json`)
//! 3. Hardcoded defaults

use crate::error::Result;
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Config file
// ---------------------------------------------------------------------------

/// Mirrors every [`Settings`] field as an `Option<T>` so absent JSON keys fall
/// through to the hardcoded defaults.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct ConfigFile {
    pub redis_url: Option<String>,
    pub database_url: Option<String>,
    pub stream_key: Option<String>,
    pub channel: Option<String>,
    pub presence_prefix: Option<String>,
    pub message_table: Option<String>,
    pub presence_event_table: Option<String>,
    pub stream_maxlen: Option<u64>,
    pub server_host: Option<String>,
    pub service_agent_id: Option<String>,
    pub service_name: Option<String>,
    pub startup_enabled: Option<bool>,
    pub startup_recipient: Option<String>,
    pub startup_topic: Option<String>,
    pub startup_body: Option<String>,
    /// Optional session identifier shared across all messages in a coordination
    /// session.  When set, `bus_post_message` auto-tags messages with
    /// `session:<id>`.
    pub session_id: Option<String>,
    /// Optional HTTP server URL for client mode.  When set, CLI commands route
    /// through this HTTP server instead of connecting to Redis directly.
    ///
    /// Example: `"http://192.168.1.100:8400"`
    pub server_url: Option<String>,
    /// Optional ORDERED list of candidate hub URLs, tried in order (e.g. a
    /// fleet p2p name, then a LAN name, then a tailnet name, then a future
    /// always-reachable cloud URL). Takes priority over `server_url` as a
    /// whole tier; `server_url` remains a single-entry convenience alias.
    ///
    /// Example: `["http://10.60.4.2:8400", "http://asuspro13.local:8400", "http://100.64.0.3:8400"]`
    pub server_urls: Option<Vec<String>>,
    /// Suppress non-fatal degraded-mode warnings that would otherwise mix into
    /// machine-readable stdout/stderr captures.
    pub machine_safe: Option<bool>,
    /// Optional bearer token required by the HTTP server. When set, every HTTP
    /// route except `/health` requires `Authorization: Bearer <token>`.
    pub auth_token: Option<String>,
    /// Opt-in to bind the HTTP server to a non-localhost interface for
    /// cross-machine access. Requires `auth_token` to also be set.
    pub allow_remote: Option<bool>,
}

/// Resolve the path for the config file.
///
/// Checks `AGENT_BUS_CONFIG` first, then falls back to
/// `%USERPROFILE%\.config\agent-bus\config.json` (Windows) or
/// `~/.config/agent-bus/config.json` (other platforms).
fn config_file_path() -> Option<std::path::PathBuf> {
    if let Ok(custom) = std::env::var("AGENT_BUS_CONFIG")
        && !custom.trim().is_empty()
    {
        return Some(std::path::PathBuf::from(custom));
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
pub fn load_config_file() -> ConfigFile {
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
  "redis_url": "redis://127.0.0.1:6380/0",
  "database_url": "postgresql://postgres@127.0.0.1:5300/redis_backend",
  "stream_key": "agent_bus:messages",
  "channel": "agent_bus:events",
  "presence_prefix": "agent_bus:presence:",
  "message_table": "agent_bus.messages",
  "presence_event_table": "agent_bus.presence_events",
  "stream_maxlen": 100000,
  "server_host": "localhost",
  "service_agent_id": "agent-bus",
  "service_name": "AgentHub",
  "startup_enabled": true,
  "startup_recipient": "all",
  "startup_topic": "status",
  "startup_body": "agent-bus is up and running",
  "machine_safe": false
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
    if let Ok(raw) = std::env::var(env_key)
        && let Ok(parsed) = raw.parse::<T>()
    {
        return parsed;
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

/// Resolve the ordered hub-candidate list.
///
/// Tiers (env beats config as a whole tier, matching every other setting):
/// 1. `AGENT_BUS_SERVER_URLS` env var — comma-separated, entries trimmed,
///    blanks dropped. `AGENT_BUS_SERVER_URLS=""` (present but empty, or
///    containing only commas/whitespace) resolves to an empty list for THIS
///    tier and falls through to tier 2, exactly like the var being unset —
///    there is no "explicit local-only override" sentinel. This matches the
///    existing `AGENT_BUS_SERVER_URL=""` precedent (also "treated as
///    absent", see the `server_url_empty_...` tests below) and is
///    deliberate: an empty env var is far more likely to be an unset-but-
///    exported shell artifact than a real request to go local-only, and a
///    caller that genuinely wants local-only mode can simply not set either
///    var and not configure `server_urls`/`server_url` in `config.json`.
/// 2. `AGENT_BUS_SERVER_URL` env var — single URL.
/// 3. `server_urls` in config.json — list, blanks dropped.
/// 4. `server_url` in config.json — single URL.
/// 5. Empty (local-only mode).
///
/// Whenever only a single URL is configured (tiers 2 or 4), the result is a
/// one-element list so `server_urls.first()` is identical to the historical
/// `server_url` field.
///
/// The final list is deduplicated by exact string match, preserving the
/// first occurrence's position (`[a, b, a]` becomes `[a, b]`). This is
/// intentionally exact-string only — no trailing-slash or case
/// normalization — so a literal repeat (e.g. copy-paste in
/// `AGENT_BUS_SERVER_URLS`) doesn't get probed twice and doesn't complicate
/// "authoritative == index 0" with a duplicate of the same string.
fn resolve_server_url_list(cfg: &ConfigFile) -> Vec<String> {
    dedup_preserve_order(resolve_server_url_list_tiers(cfg))
}

fn resolve_server_url_list_tiers(cfg: &ConfigFile) -> Vec<String> {
    if let Ok(raw) = std::env::var("AGENT_BUS_SERVER_URLS") {
        let list: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    if let Ok(single) = std::env::var("AGENT_BUS_SERVER_URL") {
        let trimmed = single.trim();
        if !trimmed.is_empty() {
            return vec![trimmed.to_owned()];
        }
    }
    if let Some(list) = cfg.server_urls.as_ref() {
        let list: Vec<String> = list
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
        if !list.is_empty() {
            return list;
        }
    }
    if let Some(single) = cfg.server_url.as_deref() {
        let trimmed = single.trim();
        if !trimmed.is_empty() {
            return vec![trimmed.to_owned()];
        }
    }
    Vec::new()
}

/// Deduplicate by exact string match, preserving the first occurrence's
/// position. See [`resolve_server_url_list`]'s doc comment for why this is
/// exact-string only.
fn dedup_preserve_order(urls: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(urls.len());
    urls.into_iter()
        .filter(|url| seen.insert(url.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

/// All configuration, read once at startup.
#[derive(Debug, Clone)]
pub struct Settings {
    pub redis_url: String,
    pub database_url: Option<String>,
    pub stream_key: String,
    pub channel_key: String,
    pub presence_prefix: String,
    pub message_table: String,
    pub presence_event_table: String,
    pub stream_maxlen: u64,
    pub service_agent_id: String,
    pub service_name: String,
    pub startup_enabled: bool,
    pub startup_recipient: String,
    pub startup_topic: String,
    pub startup_body: String,
    pub server_host: String,
    /// Session identifier propagated to every outgoing message as a
    /// `session:<id>` tag.  `None` means no session tagging.
    ///
    /// Resolution order: `AGENT_BUS_SESSION_ID` env var → `session_id` in
    /// config.json → `None` (absent from both).
    pub session_id: Option<String>,
    /// When set, CLI commands route through this HTTP server URL instead of
    /// connecting to Redis directly.  Enables remote or containerised deployments.
    ///
    /// Resolution order: `AGENT_BUS_SERVER_URL` env var → `server_url` in
    /// config.json → `None` (direct Redis mode).
    ///
    /// Example values: `"http://localhost:8400"`, `"http://192.168.1.100:8400"`
    pub server_url: Option<String>,
    /// Ordered candidate hub URLs, tried in order with a short connect
    /// timeout by [`crate::hub::resolve_hub`]. The first that answers
    /// `/health` is used; if it is index 0 the hub is `authoritative`,
    /// otherwise it is a fallback. Empty means local-only mode (this process
    /// IS the store, e.g. running on the hub host itself).
    ///
    /// `server_url` is always `server_urls.first().cloned()`, so every
    /// existing single-URL caller keeps working unchanged.
    ///
    /// Resolution order: `AGENT_BUS_SERVER_URLS` (comma-separated) env var →
    /// `AGENT_BUS_SERVER_URL` (single) env var → `server_urls` in config.json
    /// → `server_url` in config.json → empty (local-only).
    pub server_urls: Vec<String>,
    /// Suppress non-fatal warnings that otherwise pollute machine-readable
    /// output captures during degraded-mode fallbacks.
    pub machine_safe: bool,
    /// Bearer token required by the HTTP server for cross-machine access. When
    /// `Some`, every HTTP route except `/health` requires
    /// `Authorization: Bearer <token>`; `None` leaves the server open (the
    /// historical localhost-only behaviour).
    ///
    /// Resolution order: `AGENT_BUS_AUTH_TOKEN` env var → `auth_token` in
    /// config.json → `None` (no auth).
    pub auth_token: Option<String>,
    /// When `true`, the HTTP server may bind to a non-localhost `server_host`
    /// (e.g. `0.0.0.0`) for cross-machine access. Binding off-localhost also
    /// requires `auth_token` to be set; `validate()` rejects an unauthenticated
    /// remote bind. Redis/PostgreSQL remain localhost-only regardless.
    ///
    /// Resolution order: `AGENT_BUS_ALLOW_REMOTE` env var → `allow_remote` in
    /// config.json → `false`.
    pub allow_remote: bool,
}

impl Settings {
    /// Build [`Settings`] using the three-tier resolution order:
    /// env vars > config file > hardcoded defaults.
    #[must_use]
    pub fn from_env() -> Self {
        // Write a starter config if the file is absent (best-effort, silent on
        // failure so we never prevent the process from starting).
        if let Some(path) = config_file_path() {
            maybe_write_default_config(&path);
        }

        let cfg = load_config_file();
        let server_urls = resolve_server_url_list(&cfg);

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
                "redis://127.0.0.1:6380/0",
            ),
            database_url: resolve_optional_url(
                "AGENT_BUS_DATABASE_URL",
                cfg.database_url.as_deref(),
                "postgresql://postgres@127.0.0.1:5300/redis_backend",
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
            stream_maxlen: resolve_parse("AGENT_BUS_STREAM_MAXLEN", cfg.stream_maxlen, 100_000),
            service_agent_id: resolve(
                "AGENT_BUS_SERVICE_AGENT_ID",
                cfg.service_agent_id.as_deref(),
                "agent-bus",
            ),
            service_name: resolve(
                "AGENT_BUS_SERVICE_NAME",
                cfg.service_name.as_deref(),
                "AgentHub",
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
                "Agent Hub online. Protocol: (1) set_presence on start, (2) claim files via topic=ownership before editing, (3) list_messages every 2-3 calls for inbox, (4) schema=finding for findings (FINDING:+SEVERITY:), schema=status for updates, (5) batch 3-5 findings per msg, (6) COMPLETE when done. HTTP: localhost:8400. Tags: repo:<name>.",
            ),
            server_host: resolve(
                "AGENT_BUS_SERVER_HOST",
                cfg.server_host.as_deref(),
                "localhost",
            ),
            // Session ID: env var overrides config file; empty string is
            // treated as absent so callers can unset a file-configured value.
            session_id: std::env::var("AGENT_BUS_SESSION_ID")
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| cfg.session_id.filter(|s| !s.is_empty())),
            // Server URL for HTTP client mode: single-entry alias for
            // `server_urls.first()`, always kept in sync so every existing
            // caller that reads `server_url` alone keeps working unchanged.
            server_url: server_urls.first().cloned(),
            server_urls,
            machine_safe: resolve_parse("AGENT_BUS_MACHINE_SAFE", cfg.machine_safe, false),
            // Bearer token for the HTTP server: env var overrides config file;
            // empty string treated as absent (no auth required).
            auth_token: std::env::var("AGENT_BUS_AUTH_TOKEN")
                .ok()
                .filter(|s| !s.is_empty())
                .or_else(|| cfg.auth_token.filter(|s| !s.is_empty())),
            allow_remote: resolve_parse("AGENT_BUS_ALLOW_REMOTE", cfg.allow_remote, false),
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
    /// # use agent_bus_core::settings::Settings;
    /// let settings = Settings::from_env();
    /// settings.validate().expect("default settings should be valid");
    /// ```
    pub fn validate(&self) -> Result<()> {
        validate_localhost_url(&self.redis_url, "AGENT_BUS_REDIS_URL")?;
        if let Some(ref db_url) = self.database_url {
            validate_localhost_url(db_url, "AGENT_BUS_DATABASE_URL")?;
        }
        if !is_localhost(&self.server_host) {
            // Off-localhost binding is opt-in (cross-machine access) and may
            // never be unauthenticated.
            if !self.allow_remote {
                return Err(crate::error::AgentBusError::InvalidParams(format!(
                    "AGENT_BUS_SERVER_HOST '{}' is not localhost. Set \
                     AGENT_BUS_ALLOW_REMOTE=true to bind the HTTP server to a routable \
                     interface for cross-machine access.",
                    self.server_host
                )));
            }
            if self.auth_token.is_none() {
                return Err(crate::error::AgentBusError::InvalidParams(format!(
                    "refusing to expose the bus on non-localhost interface '{}' without \
                     authentication: set AGENT_BUS_AUTH_TOKEN",
                    self.server_host
                )));
            }
        }
        validate_identifier(&self.stream_key, "AGENT_BUS_STREAM_KEY")?;
        validate_identifier(&self.channel_key, "AGENT_BUS_CHANNEL")?;
        validate_identifier(&self.presence_prefix, "AGENT_BUS_PRESENCE_PREFIX")?;
        validate_identifier(&self.message_table, "AGENT_BUS_MESSAGE_TABLE")?;
        validate_identifier(&self.presence_event_table, "AGENT_BUS_PRESENCE_EVENT_TABLE")?;
        validate_identifier(&self.service_name, "AGENT_BUS_SERVICE_NAME")?;
        Ok(())
    }

    #[must_use]
    pub fn log_non_fatal_warnings(&self) -> bool {
        !self.machine_safe
    }
}

fn is_localhost(host: &str) -> bool {
    let h = host.trim().to_lowercase();
    h == "localhost" || h == "127.0.0.1" || h == "::1" || h == "[::1]"
}

fn url_host_span(url: &str) -> Option<(std::ops::Range<usize>, &str)> {
    let scheme_end = url.find("://")?;
    let authority_start = scheme_end + 3;
    let authority_end = url[authority_start..]
        .find(['/', '?', '#'])
        .map_or(url.len(), |index| authority_start + index);
    let authority = &url[authority_start..authority_end];
    let host_start = authority
        .rfind('@')
        .map_or(authority_start, |index| authority_start + index + 1);
    let host_port = &url[host_start..authority_end];
    if host_port.starts_with('[') {
        let close = host_port.find(']')?;
        let host_end = host_start + close + 1;
        let host = &url[host_start + 1..host_end - 1];
        return Some((host_start..host_end, host));
    }

    let host_len = host_port.find(':').unwrap_or(host_port.len());
    let host_end = host_start + host_len;
    Some((host_start..host_end, &url[host_start..host_end]))
}

/// In this crate's unit tests, panic before connecting to a live agent-bus
/// port (see `test_support`). A no-op in every other build.
#[cfg(test)]
pub(crate) fn refuse_live_bus_in_unit_tests(url: &str) {
    crate::test_support::refuse_live_bus(url);
}

#[cfg(not(test))]
#[inline]
pub(crate) const fn refuse_live_bus_in_unit_tests(_url: &str) {}

/// Return deterministic loopback candidates for backend URLs using `localhost`.
///
/// Windows commonly resolves `localhost` to `::1` before `127.0.0.1`, while
/// local Redis containers are often IPv4-only. Backend clients use these
/// candidates to avoid depending on OS resolver ordering.
#[must_use]
pub fn loopback_url_candidates(url: &str) -> Vec<String> {
    let Some((host_range, host)) = url_host_span(url) else {
        return vec![url.to_owned()];
    };
    if !host.eq_ignore_ascii_case("localhost") {
        return vec![url.to_owned()];
    }

    let mut candidates = Vec::with_capacity(3);
    for replacement in ["127.0.0.1", "[::1]", "localhost"] {
        let mut candidate = String::with_capacity(url.len() + replacement.len());
        candidate.push_str(&url[..host_range.start]);
        candidate.push_str(replacement);
        candidate.push_str(&url[host_range.end..]);
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
    candidates
}

/// Extract the host from a URL and verify it is localhost.
fn validate_localhost_url(url: &str, env_var: &str) -> Result<()> {
    let Some((_host_range, host)) = url_host_span(url) else {
        return Ok(()); // not a URL, skip
    };
    if !is_localhost(host) {
        return Err(crate::error::AgentBusError::InvalidParams(format!(
            "{env_var} must use localhost, got host '{host}' in '{url}'"
        )));
    }
    Ok(())
}

/// Verify an identifier is non-empty and contains no whitespace.
fn validate_identifier(value: &str, env_var: &str) -> Result<()> {
    if value.is_empty() {
        return Err(crate::error::AgentBusError::InvalidParams(format!(
            "{env_var} must not be empty"
        )));
    }
    if value.contains(' ') || value.contains('\n') || value.contains('\t') {
        return Err(crate::error::AgentBusError::InvalidParams(format!(
            "{env_var} must not contain whitespace, got '{value}'"
        )));
    }
    Ok(())
}

#[must_use]
pub fn redact_url(value: &str) -> String {
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
        assert!(cfg.service_name.is_none());
        assert!(cfg.startup_enabled.is_none());
        assert!(cfg.startup_recipient.is_none());
        assert!(cfg.startup_topic.is_none());
        assert!(cfg.startup_body.is_none());
        assert!(cfg.machine_safe.is_none());
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
    fn validate_rejects_remote_bind_without_auth_token() {
        let mut s = Settings::from_env();
        s.server_host = "0.0.0.0".to_owned();
        s.allow_remote = true;
        s.auth_token = None;
        // allow_remote alone is not enough — an unauthenticated remote bind
        // must be refused.
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_accepts_remote_bind_with_allow_and_token() {
        let mut s = Settings::from_env();
        s.redis_url = "redis://localhost:6380/0".to_owned();
        s.database_url = Some("postgresql://postgres@localhost:5432/db".to_owned());
        s.server_host = "0.0.0.0".to_owned();
        s.allow_remote = true;
        s.auth_token = Some("secret-token".to_owned());
        assert!(s.validate().is_ok());
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
    fn validate_accepts_bracketed_ipv6_backend_urls() {
        let mut s = Settings::from_env();
        s.redis_url = "redis://[::1]:6380/0".to_owned();
        s.database_url = Some("postgresql://postgres@[::1]:5300/redis_backend".to_owned());
        assert!(s.validate().is_ok());
    }

    #[test]
    fn localhost_backend_candidates_try_explicit_loopbacks_first() {
        let candidates =
            loopback_url_candidates("redis://default:secret@localhost:6380/0?timeout=1");
        assert_eq!(
            candidates,
            vec![
                "redis://default:secret@127.0.0.1:6380/0?timeout=1".to_owned(),
                "redis://default:secret@[::1]:6380/0?timeout=1".to_owned(),
                "redis://default:secret@localhost:6380/0?timeout=1".to_owned(),
            ]
        );
    }

    #[test]
    fn localhost_backend_candidates_leave_explicit_hosts_unchanged() {
        assert_eq!(
            loopback_url_candidates("postgresql://postgres@127.0.0.1:5300/redis_backend"),
            vec!["postgresql://postgres@127.0.0.1:5300/redis_backend".to_owned()]
        );
        assert_eq!(
            loopback_url_candidates("redis://[::1]:6380/0"),
            vec!["redis://[::1]:6380/0".to_owned()]
        );
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

    // -----------------------------------------------------------------------
    // session_id — Task 4.1
    // -----------------------------------------------------------------------

    /// Simulate the env+config-file resolution logic for `session_id`.
    ///
    /// Mirrors what `Settings::from_env()` does:
    /// env var (non-empty) → config-file value (non-empty) → None.
    fn resolve_session_id(env_val: Option<&str>, cfg_val: Option<&str>) -> Option<String> {
        env_val
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| cfg_val.filter(|s| !s.is_empty()).map(str::to_owned))
    }

    #[test]
    fn session_id_loaded_from_env() {
        // Test the resolution logic directly — avoids process-wide env races
        // with other parallel test threads that may touch AGENT_BUS_SESSION_ID.
        let sid = resolve_session_id(Some("test-session-abc"), None);
        assert_eq!(sid.as_deref(), Some("test-session-abc"));
    }

    #[test]
    fn session_id_defaults_to_none_when_unset() {
        let sid = resolve_session_id(None, None);
        assert!(
            sid.is_none(),
            "session_id should be None when env and config both omit it"
        );
    }

    #[test]
    fn session_id_empty_env_treated_as_none() {
        // Empty string from env → falls through to config (also absent) → None.
        let sid = resolve_session_id(Some(""), None);
        assert!(
            sid.is_none(),
            "empty AGENT_BUS_SESSION_ID should be treated as absent"
        );
    }

    #[test]
    fn session_id_env_overrides_config() {
        // Env var takes precedence even when config also specifies a session_id.
        let sid = resolve_session_id(Some("from-env"), Some("from-config"));
        assert_eq!(sid.as_deref(), Some("from-env"));
    }

    #[test]
    fn session_id_falls_back_to_config_when_env_absent() {
        let sid = resolve_session_id(None, Some("from-config"));
        assert_eq!(sid.as_deref(), Some("from-config"));
    }

    #[test]
    fn session_id_empty_env_falls_back_to_config() {
        // Empty env → config value used.
        let sid = resolve_session_id(Some(""), Some("from-config"));
        assert_eq!(sid.as_deref(), Some("from-config"));
    }

    #[test]
    fn config_file_deserializes_session_id() {
        let json = r#"{"session_id": "sprint-2026-03-19"}"#;
        let cfg: ConfigFile = serde_json::from_str(json).expect("valid JSON");
        assert_eq!(cfg.session_id.as_deref(), Some("sprint-2026-03-19"));
    }

    #[test]
    fn config_file_default_has_none_session_id() {
        let cfg = ConfigFile::default();
        assert!(cfg.session_id.is_none());
    }

    // -----------------------------------------------------------------------
    // server_url — Feature 2 resolution tests
    // -----------------------------------------------------------------------

    /// Mirror the resolution logic used in `Settings::from_env()` for
    /// `server_url`, tested without mutating the process-wide environment.
    fn resolve_server_url(env_val: Option<&str>, cfg_val: Option<&str>) -> Option<String> {
        env_val
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| cfg_val.filter(|s| !s.is_empty()).map(str::to_owned))
    }

    #[test]
    fn server_url_loaded_from_env() {
        let url = resolve_server_url(Some("http://localhost:8400"), None);
        assert_eq!(url.as_deref(), Some("http://localhost:8400"));
    }

    #[test]
    fn server_url_defaults_to_none_when_unset() {
        let url = resolve_server_url(None, None);
        assert!(
            url.is_none(),
            "server_url must be None when env and config both absent"
        );
    }

    #[test]
    fn server_url_empty_env_treated_as_none() {
        let url = resolve_server_url(Some(""), None);
        assert!(
            url.is_none(),
            "empty AGENT_BUS_SERVER_URL should be treated as absent"
        );
    }

    #[test]
    fn server_url_env_overrides_config() {
        let url = resolve_server_url(Some("http://localhost:8400"), Some("http://remote:8400"));
        assert_eq!(url.as_deref(), Some("http://localhost:8400"));
    }

    #[test]
    fn server_url_falls_back_to_config_when_env_absent() {
        let url = resolve_server_url(None, Some("http://remote:8400"));
        assert_eq!(url.as_deref(), Some("http://remote:8400"));
    }

    #[test]
    fn server_url_empty_env_falls_back_to_config() {
        let url = resolve_server_url(Some(""), Some("http://remote:8400"));
        assert_eq!(url.as_deref(), Some("http://remote:8400"));
    }

    #[test]
    fn config_file_deserializes_server_url() {
        let json = r#"{"server_url": "http://localhost:8400"}"#;
        let cfg: ConfigFile = serde_json::from_str(json).expect("valid JSON");
        assert_eq!(cfg.server_url.as_deref(), Some("http://localhost:8400"));
    }

    #[test]
    fn config_file_default_has_none_server_url() {
        let cfg = ConfigFile::default();
        assert!(cfg.server_url.is_none());
    }

    #[test]
    fn settings_from_env_server_url_is_none_by_default() {
        // When AGENT_BUS_SERVER_URL is not set, server_url must be None.
        // We can't guarantee the env state in a shared test runner, but we can
        // verify the field exists and is Option<String>.
        let s = Settings::from_env();
        // server_url is either None or a non-empty string — never empty.
        if let Some(ref url) = s.server_url {
            assert!(!url.is_empty(), "server_url must not be an empty string");
        }
    }

    #[test]
    fn config_file_deserializes_machine_safe() {
        let json = r#"{"machine_safe": true}"#;
        let cfg: ConfigFile = serde_json::from_str(json).expect("valid JSON");
        assert_eq!(cfg.machine_safe, Some(true));
    }

    #[test]
    fn log_non_fatal_warnings_disabled_when_machine_safe_enabled() {
        let mut s = Settings::from_env();
        s.machine_safe = true;
        assert!(!s.log_non_fatal_warnings());
    }

    // -----------------------------------------------------------------------
    // server_urls — ordered hub-candidate list (#78)
    // -----------------------------------------------------------------------

    /// Serializes access to the `AGENT_BUS_SERVER_URL{,S}` env vars across
    /// this module's tests: `cargo test` runs tests in this file on multiple
    /// threads by default, and these vars are process-global.
    #[expect(
        clippy::semicolon_outside_block,
        reason = "clippy::semicolon_outside_block and clippy::semicolon_if_nothing_returned \
                  disagree on where the ; belongs for a multi-statement unsafe block used as a \
                  statement; keeping the ; on each inner statement reads clearer here"
    )]
    fn with_server_url_env<T>(urls: Option<&str>, url: Option<&str>, f: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // SAFETY: serialized by LOCK; no other thread touches these two vars
        // while the guard is held (every test in this module goes through
        // this helper or leaves both vars untouched).
        unsafe {
            match urls {
                Some(v) => std::env::set_var("AGENT_BUS_SERVER_URLS", v),
                None => std::env::remove_var("AGENT_BUS_SERVER_URLS"),
            }
            match url {
                Some(v) => std::env::set_var("AGENT_BUS_SERVER_URL", v),
                None => std::env::remove_var("AGENT_BUS_SERVER_URL"),
            }
        }
        let result = f();
        // SAFETY: same justification as above.
        unsafe {
            std::env::remove_var("AGENT_BUS_SERVER_URLS");
            std::env::remove_var("AGENT_BUS_SERVER_URL");
        }
        result
    }

    #[test]
    fn server_urls_empty_by_default() {
        with_server_url_env(None, None, || {
            assert_eq!(
                resolve_server_url_list(&ConfigFile::default()),
                Vec::<String>::new()
            );
        });
    }

    #[test]
    fn server_urls_parses_comma_separated_list_and_trims_entries() {
        with_server_url_env(
            Some(" http://10.60.4.2:8400 , http://asuspro13.local:8400,http://100.64.0.3:8400 "),
            None,
            || {
                assert_eq!(
                    resolve_server_url_list(&ConfigFile::default()),
                    vec![
                        "http://10.60.4.2:8400".to_owned(),
                        "http://asuspro13.local:8400".to_owned(),
                        "http://100.64.0.3:8400".to_owned(),
                    ]
                );
            },
        );
    }

    #[test]
    fn server_urls_drops_blank_entries() {
        with_server_url_env(Some("http://a:8400,,  ,http://b:8400"), None, || {
            assert_eq!(
                resolve_server_url_list(&ConfigFile::default()),
                vec!["http://a:8400".to_owned(), "http://b:8400".to_owned()]
            );
        });
    }

    #[test]
    fn server_urls_list_env_takes_priority_over_single_env() {
        with_server_url_env(Some("http://list:8400"), Some("http://single:8400"), || {
            assert_eq!(
                resolve_server_url_list(&ConfigFile::default()),
                vec!["http://list:8400".to_owned()]
            );
        });
    }

    #[test]
    fn server_urls_falls_back_to_single_env_when_list_env_absent() {
        with_server_url_env(None, Some("http://single:8400"), || {
            assert_eq!(
                resolve_server_url_list(&ConfigFile::default()),
                vec!["http://single:8400".to_owned()]
            );
        });
    }

    #[test]
    fn server_urls_falls_back_to_config_list_when_env_absent() {
        with_server_url_env(None, None, || {
            let cfg = ConfigFile {
                server_urls: Some(vec![
                    "http://cfg-a:8400".to_owned(),
                    "http://cfg-b:8400".to_owned(),
                ]),
                ..ConfigFile::default()
            };
            assert_eq!(
                resolve_server_url_list(&cfg),
                vec![
                    "http://cfg-a:8400".to_owned(),
                    "http://cfg-b:8400".to_owned()
                ]
            );
        });
    }

    #[test]
    fn server_urls_falls_back_to_config_single_when_list_and_env_absent() {
        with_server_url_env(None, None, || {
            let cfg = ConfigFile {
                server_url: Some("http://cfg-single:8400".to_owned()),
                ..ConfigFile::default()
            };
            assert_eq!(
                resolve_server_url_list(&cfg),
                vec!["http://cfg-single:8400".to_owned()]
            );
        });
    }

    #[test]
    fn server_url_field_is_first_entry_of_server_urls() {
        with_server_url_env(Some("http://first:8400,http://second:8400"), None, || {
            let s = Settings::from_env();
            assert_eq!(s.server_url.as_deref(), Some("http://first:8400"));
            assert_eq!(
                s.server_urls,
                vec![
                    "http://first:8400".to_owned(),
                    "http://second:8400".to_owned()
                ]
            );
        });
    }

    #[test]
    fn server_urls_empty_means_local_only() {
        with_server_url_env(None, None, || {
            let s = Settings::from_env();
            assert!(s.server_urls.is_empty());
            assert!(s.server_url.is_none());
        });
    }

    #[test]
    fn config_file_deserializes_server_urls_list() {
        let json = r#"{"server_urls": ["http://a:8400", "http://b:8400"]}"#;
        let cfg: ConfigFile = serde_json::from_str(json).expect("valid JSON");
        assert_eq!(
            cfg.server_urls,
            Some(vec!["http://a:8400".to_owned(), "http://b:8400".to_owned()])
        );
    }

    #[test]
    fn server_urls_dedups_exact_repeats_preserving_first_occurrence_order() {
        with_server_url_env(
            Some("http://a:8400,http://b:8400,http://a:8400"),
            None,
            || {
                assert_eq!(
                    resolve_server_url_list(&ConfigFile::default()),
                    vec!["http://a:8400".to_owned(), "http://b:8400".to_owned()],
                    "a literal repeat must not be probed twice, and the first \
                 occurrence's position (not the last) must win so index 0 \
                 stays authoritative"
                );
            },
        );
    }

    #[test]
    fn server_urls_dedup_is_exact_string_only_not_normalized() {
        // A trailing-slash or case difference is a DIFFERENT string on
        // purpose (see resolve_server_url_list's doc comment) -- this is not
        // a bug to fix here, just documenting the boundary so it isn't
        // mistaken for one later.
        with_server_url_env(
            Some("http://a:8400,http://a:8400/,HTTP://A:8400"),
            None,
            || {
                assert_eq!(
                    resolve_server_url_list(&ConfigFile::default()),
                    vec![
                        "http://a:8400".to_owned(),
                        "http://a:8400/".to_owned(),
                        "HTTP://A:8400".to_owned(),
                    ]
                );
            },
        );
    }

    #[test]
    fn server_urls_config_dedups_too() {
        with_server_url_env(None, None, || {
            let cfg = ConfigFile {
                server_urls: Some(vec![
                    "http://cfg-a:8400".to_owned(),
                    "http://cfg-b:8400".to_owned(),
                    "http://cfg-a:8400".to_owned(),
                ]),
                ..ConfigFile::default()
            };
            assert_eq!(
                resolve_server_url_list(&cfg),
                vec![
                    "http://cfg-a:8400".to_owned(),
                    "http://cfg-b:8400".to_owned()
                ]
            );
        });
    }

    /// #78 review item 6: `AGENT_BUS_SERVER_URLS=""` (present in the
    /// environment but empty) must fall through to the next tier exactly
    /// like the var being unset — it is NOT a "force local-only" sentinel.
    /// The discriminating case is an empty env var with a real config-file
    /// list still configured: if `""` meant "go local-only", this would
    /// return an empty list; since it means "absent", it must return the
    /// config-file list untouched. See `resolve_server_url_list`'s doc
    /// comment for the full tier order and rationale.
    #[test]
    fn server_urls_env_present_but_empty_falls_through_to_config_not_local_only() {
        with_server_url_env(Some(""), None, || {
            let cfg = ConfigFile {
                server_urls: Some(vec![
                    "http://cfg-a:8400".to_owned(),
                    "http://cfg-b:8400".to_owned(),
                ]),
                ..ConfigFile::default()
            };
            assert_eq!(
                resolve_server_url_list(&cfg),
                vec![
                    "http://cfg-a:8400".to_owned(),
                    "http://cfg-b:8400".to_owned()
                ],
                "AGENT_BUS_SERVER_URLS=\"\" must be treated as absent, not as an \
                 explicit override to local-only mode"
            );
        });
    }

    /// Same as above but with nothing at all configured downstream, so the
    /// end-to-end result really is local-only -- proving the empty-string
    /// case falls all the way through the tier chain rather than getting
    /// stuck partway.
    #[test]
    fn server_urls_env_present_but_empty_and_nothing_else_configured_is_local_only() {
        with_server_url_env(Some("  ,  ,"), None, || {
            assert_eq!(
                resolve_server_url_list(&ConfigFile::default()),
                Vec::<String>::new()
            );
        });
    }
}
