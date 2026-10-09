//! Environment-variable and config-file configuration, read once at startup.
//!
//! Resolution order (highest wins):
//! 1. Environment variables
//! 2. Config file (`AGENT_BUS_CONFIG` env var path, or `~/.config/agent-bus/config.json`)
//! 3. Hardcoded defaults

use crate::error::Result;
use crate::hub_candidates::{
    CandidateSpec, HubCandidate, build_candidates, parse_candidate_entries, parse_candidate_json,
    validate_candidates,
};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Write;

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
    #[serde(
        default,
        deserialize_with = "crate::hub_candidates::deserialize_candidate_value"
    )]
    pub server_urls: Option<serde_json::Value>,
    pub probe_connect_timeout_ms: Option<u64>,
    pub hub_cache_ttl_seconds: Option<u64>,
    /// Named network locations used to order hub candidates by their
    /// `sites`; see [`crate::network_location`].
    pub network_locations: Option<serde_json::Value>,
    /// The identity this process reports as `hub_identity` in `/health`
    /// when it serves as a hub, so clients can verify a route's `hub`.
    pub hub_identity: Option<String>,
    #[serde(skip)]
    pub configuration_error: Option<String>,
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
pub(crate) fn config_file_path() -> Option<std::path::PathBuf> {
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

/// Load configuration while retaining read and parsing failures.
#[must_use]
pub fn load_config_file() -> ConfigFile {
    let Some(path) = config_file_path() else {
        return ConfigFile::default();
    };
    load_config_file_at(&path)
}

fn load_config_file_at(path: &std::path::Path) -> ConfigFile {
    match std::fs::read_to_string(path) {
        Ok(text) => match serde_json::from_str::<ConfigFile>(&text) {
            Ok(cfg) => {
                tracing::debug!("loaded agent-bus config from {}", path.display());
                cfg
            }
            Err(err) => {
                tracing::debug!(
                    "agent-bus config at {} could not be parsed (line {}, column {}); refusing backend access",
                    path.display(),
                    err.line(),
                    err.column()
                );
                ConfigFile {
                    configuration_error: Some(format!(
                        "invalid agent-bus configuration (line {}, column {})",
                        err.line(),
                        err.column()
                    )),
                    ..ConfigFile::default()
                }
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => ConfigFile::default(),
        Err(_) => ConfigFile {
            configuration_error: Some("agent-bus configuration could not be read".to_owned()),
            ..ConfigFile::default()
        },
    }
}

/// Write a default config file if one does not already exist.
fn maybe_write_default_config(path: &std::path::Path) -> std::io::Result<()> {
    write_default_config_before_open(path, |_| Ok(()))
}

// The synchronization boundary permits a real concurrent-creator regression;
// the exclusive open, write and flush below always remain the actual writer.
fn write_default_config_before_open(
    path: &std::path::Path,
    before_open: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
) -> std::io::Result<()> {
    // Retain read-only existing-config behavior; CreateNew still protects the
    // absence-check race, and try_exists exposes real metadata failures.
    if path.try_exists()? {
        return Ok(());
    }
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    before_open(path)?;
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(()),
        Err(error) => return Err(error),
    };
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
    file.write_all(default_json.as_bytes())?;
    file.sync_all()?;
    tracing::debug!("wrote default agent-bus config to {}", path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// 3-tier resolver helpers
// ---------------------------------------------------------------------------

fn load_settings_config() -> ConfigFile {
    config_file_path().map_or_else(ConfigFile::default, |path| load_settings_config_at(&path))
}

fn load_settings_config_at(path: &std::path::Path) -> ConfigFile {
    if let Err(error) = maybe_write_default_config(path) {
        return ConfigFile {
            configuration_error: Some(format!(
                "agent-bus configuration could not be initialized ({:?})",
                error.kind()
            )),
            ..ConfigFile::default()
        };
    }
    load_config_file_at(path)
}

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

fn resolve_nonempty(env_key: &str, config_value: Option<String>) -> Option<String> {
    std::env::var(env_key)
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| config_value.filter(|value| !value.is_empty()))
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
#[cfg(test)]
fn resolve_server_url_list(cfg: &ConfigFile) -> Vec<String> {
    dedup_preserve_order(resolve_server_url_list_tiers(cfg))
}

#[cfg(test)]
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
        let list: Vec<String> = parse_candidate_entries(list)
            .unwrap_or_default()
            .into_iter()
            .map(|spec| spec.url)
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
#[cfg(test)]
fn dedup_preserve_order(urls: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::with_capacity(urls.len());
    urls.into_iter()
        .filter(|url| seen.insert(url.clone()))
        .collect()
}

/// Select an entire credential tier before resolving individual entries.
fn resolve_candidate_settings(cfg: &ConfigFile) -> (Vec<HubCandidate>, Option<String>) {
    let json = std::env::var("AGENT_BUS_SERVER_CANDIDATES").ok();
    let list = std::env::var("AGENT_BUS_SERVER_URLS").ok();
    let single = std::env::var("AGENT_BUS_SERVER_URL").ok();
    resolve_candidate_tiers(cfg, json.as_deref(), list.as_deref(), single.as_deref())
}

fn legacy_specs(urls: impl IntoIterator<Item = String>) -> Vec<CandidateSpec> {
    urls.into_iter()
        .map(|url| CandidateSpec {
            url,
            role: None,
            auth: crate::hub_candidates::CandidateAuth::Global,
            sites: Vec::new(),
            hub: None,
        })
        .collect()
}

fn resolve_candidate_tiers(
    cfg: &ConfigFile,
    json: Option<&str>,
    list: Option<&str>,
    single: Option<&str>,
) -> (Vec<HubCandidate>, Option<String>) {
    if let Some(error) = &cfg.configuration_error {
        return (Vec::new(), Some(error.clone()));
    }
    let list = list
        .map(|raw| {
            raw.split(',')
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let single = single.map(str::trim).filter(|url| !url.is_empty());
    let parsed = if let Some(raw) = json {
        parse_candidate_json(raw).and_then(|entries| {
            if entries.is_empty() {
                Err("AGENT_BUS_SERVER_CANDIDATES must contain at least one hub".to_owned())
            } else {
                Ok(entries)
            }
        })
    } else if !list.is_empty() {
        Ok(legacy_specs(list))
    } else if let Some(url) = single {
        Ok(legacy_specs([url.to_owned()]))
    } else if let Some(raw) = &cfg.server_urls {
        parse_candidate_entries(raw).map(|entries| {
            if entries.is_empty() {
                legacy_specs(
                    cfg.server_url
                        .as_deref()
                        .map(str::trim)
                        .filter(|url| !url.is_empty())
                        .map(str::to_owned),
                )
            } else {
                entries
            }
        })
    } else {
        Ok(legacy_specs(
            cfg.server_url
                .as_deref()
                .map(str::trim)
                .filter(|url| !url.is_empty())
                .map(str::to_owned),
        ))
    };
    let entries = match parsed {
        Ok(entries) => entries,
        Err(error) => return (Vec::new(), Some(error)),
    };
    let mut specs = BTreeMap::new();
    let mut urls = Vec::new();
    for entry in entries {
        if let Some(existing) = specs.get(&entry.url) {
            if existing != &entry {
                return (
                    Vec::new(),
                    Some("conflicting hub candidate definitions".to_owned()),
                );
            }
        } else {
            urls.push(entry.url.clone());
            specs.insert(entry.url.clone(), entry);
        }
    }
    let candidates = build_candidates(&urls, &specs);
    let error = validate_candidates(&candidates).err();
    (candidates, error)
}

/// Parse `network_locations`; an invalid rule set is reported, not fatal.
fn resolve_location_settings(
    cfg: &ConfigFile,
) -> (Vec<crate::network_location::LocationRule>, Option<String>) {
    cfg.network_locations
        .as_ref()
        .map_or(
            (Vec::new(), None),
            |raw| match crate::network_location::parse_location_rules(raw) {
                Ok(rules) => (rules, None),
                Err(error) => (Vec::new(), Some(error)),
            },
        )
}

/// `AGENT_BUS_HUB_IDENTITY` → `hub_identity`. Clients compare it exactly, so
/// a value they could never match (bad characters, stray spaces) is dropped
/// rather than served.
fn resolve_hub_identity(config_value: Option<String>) -> Option<String> {
    resolve_nonempty("AGENT_BUS_HUB_IDENTITY", config_value).filter(|identity| {
        let valid = crate::network_location::valid_site_name(identity);
        if !valid {
            tracing::warn!(
                "hub_identity is not [A-Za-z0-9._-]{{1,64}}; not reporting it, so clients \
                 with a named `hub` route will reject this hub"
            );
        }
        valid
    })
}

fn candidate_urls(candidates: &[HubCandidate]) -> Vec<String> {
    candidates
        .iter()
        .map(|candidate| candidate.url.clone())
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
    /// First URL from the resolved candidate tier; see `hub_candidates`.
    ///
    /// Example values: `"http://localhost:8400"`, `"http://192.168.1.100:8400"`
    pub server_url: Option<String>,
    /// Ordered compatibility URL projection of the resolved candidate tier.
    /// [`crate::hub::resolve_configured_hub`] uses each candidate's credential
    /// source and role. Empty means local-only mode (this process
    /// IS the store, e.g. running on the hub host itself).
    ///
    /// `server_url` is always `server_urls.first().cloned()`, so every
    /// existing single-URL caller keeps working unchanged.
    ///
    /// Resolution order: `AGENT_BUS_SERVER_CANDIDATES` JSON env var →
    /// `AGENT_BUS_SERVER_URLS` (comma-separated) env var →
    /// `AGENT_BUS_SERVER_URL` (single) env var → `server_urls` in config.json
    /// → `server_url` in config.json → empty (local-only).
    pub server_urls: Vec<String>,
    /// Resolved credential sources and roles for the selected configuration tier.
    pub hub_candidates: Vec<HubCandidate>,
    /// Maximum time spent connecting to one hub.
    pub probe_connect_timeout_ms: u64,
    /// Cross-process resolution cache lifetime; zero disables it.
    pub hub_cache_ttl_seconds: u64,
    /// Invalid configuration must fail closed before any backend operation.
    pub hub_config_error: Option<String>,
    /// Location rules from `network_locations`; empty disables detection
    /// unless `AGENT_BUS_NETWORK_LOCATION` is set.
    pub network_locations: Vec<crate::network_location::LocationRule>,
    /// Why `network_locations` was ignored, if it was invalid. Location only
    /// orders probes, so a bad rule set degrades to configured order and is
    /// reported in `health` instead of taking the client offline.
    pub network_location_error: Option<String>,
    /// Identity reported as `hub_identity` by `/health`.
    ///
    /// Resolution order: `AGENT_BUS_HUB_IDENTITY` env var → `hub_identity`
    /// in config.json → `None` (not reported).
    pub hub_identity: Option<String>,
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
    /// Construct deterministic unit-test settings without reading host configuration.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            redis_url: "redis://127.0.0.1:1/0".to_owned(),
            database_url: Some("postgresql://postgres@127.0.0.1:1/offline".to_owned()),
            stream_key: "agent_bus:messages".to_owned(),
            channel_key: "agent_bus:events".to_owned(),
            presence_prefix: "agent_bus:presence:".to_owned(),
            message_table: "agent_bus.messages".to_owned(),
            presence_event_table: "agent_bus.presence_events".to_owned(),
            stream_maxlen: 100_000,
            service_agent_id: "agent-bus".to_owned(),
            service_name: "AgentHub".to_owned(),
            startup_enabled: false,
            startup_recipient: "all".to_owned(),
            startup_topic: "status".to_owned(),
            startup_body: "unit test fixture".to_owned(),
            server_host: "localhost".to_owned(),
            session_id: None,
            server_url: None,
            server_urls: Vec::new(),
            hub_candidates: Vec::new(),
            probe_connect_timeout_ms: 750,
            hub_cache_ttl_seconds: 0,
            hub_config_error: None,
            network_locations: Vec::new(),
            network_location_error: None,
            hub_identity: None,
            machine_safe: false,
            auth_token: None,
            allow_remote: false,
        }
    }

    /// Build [`Settings`] using the three-tier resolution order:
    /// env vars > config file > hardcoded defaults.
    #[must_use]
    pub fn from_env() -> Self {
        // Exclusively create an absent starter config. Genuine initialization
        // failures are retained for validation before any backend access.
        let cfg = load_settings_config();
        let (hub_candidates, hub_config_error) = resolve_candidate_settings(&cfg);
        let (network_locations, network_location_error) = resolve_location_settings(&cfg);
        let server_urls = candidate_urls(&hub_candidates);

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
            session_id: resolve_nonempty("AGENT_BUS_SESSION_ID", cfg.session_id),
            // Server URL for HTTP client mode: single-entry alias for
            // `server_urls.first()`, always kept in sync so every existing
            // caller that reads `server_url` alone keeps working unchanged.
            server_url: server_urls.first().cloned(),
            server_urls,
            hub_candidates,
            hub_config_error,
            network_locations,
            network_location_error,
            hub_identity: resolve_hub_identity(cfg.hub_identity),
            probe_connect_timeout_ms: resolve_parse(
                "AGENT_BUS_PROBE_CONNECT_TIMEOUT_MS",
                cfg.probe_connect_timeout_ms,
                750,
            ),
            hub_cache_ttl_seconds: resolve_parse(
                "AGENT_BUS_HUB_CACHE_TTL_SECONDS",
                cfg.hub_cache_ttl_seconds,
                60,
            ),
            machine_safe: resolve_parse("AGENT_BUS_MACHINE_SAFE", cfg.machine_safe, false),
            // Bearer token for the HTTP server: env var overrides config file;
            // empty string treated as absent (no auth required).
            auth_token: resolve_nonempty("AGENT_BUS_AUTH_TOKEN", cfg.auth_token),
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
        if let Some(error) = &self.hub_config_error {
            return Err(crate::error::AgentBusError::InvalidParams(error.clone()));
        }
        if self.probe_connect_timeout_ms == 0 || self.probe_connect_timeout_ms > 30_000 {
            return Err(crate::error::AgentBusError::InvalidParams(
                "probe_connect_timeout_ms must be between 1 and 30000".to_owned(),
            ));
        }
        validate_candidates(&self.effective_hub_candidates())
            .map_err(crate::error::AgentBusError::InvalidParams)?;
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

    /// Preserve callers that update the legacy URL list after construction.
    #[must_use]
    pub fn effective_hub_candidates(&self) -> Vec<HubCandidate> {
        let urls = self
            .hub_candidates
            .iter()
            .map(|candidate| candidate.url.as_str())
            .collect::<Vec<_>>();
        if urls
            == self
                .server_urls
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
        {
            self.hub_candidates.clone()
        } else {
            build_candidates(&self.server_urls, &BTreeMap::new())
        }
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

    #[test]
    fn starter_config_is_valid_json_and_preserves_existing_bytes() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let path = directory.path().join("nested/config.json");
        maybe_write_default_config(&path).expect("starter config initialized");
        let bytes = std::fs::read(&path).expect("starter config created");
        let config: ConfigFile =
            serde_json::from_slice(&bytes).expect("starter config must remain valid JSON");
        assert_eq!(config.service_name.as_deref(), Some("AgentHub"));
        assert_eq!(config.server_host.as_deref(), Some("localhost"));
        std::fs::write(&path, b"existing configuration bytes").expect("existing fixture");
        maybe_write_default_config(&path).expect("existing creator preserved");
        assert_eq!(
            std::fs::read(&path).expect("existing config preserved"),
            b"existing configuration bytes"
        );
    }

    #[test]
    fn starter_config_preserves_creator_between_parent_setup_and_exclusive_open() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let path = directory.path().join("nested/config.json");
        let mut creator = None;
        let mut calls = 0;
        // Deterministically interleave the external creator immediately before
        // the real exclusive open. Its original file remains held until the
        // actual writer returns. No thread, rendezvous or join can hang if
        // this callback is omitted or returns an error.
        let result = write_default_config_before_open(&path, |actual_path| {
            assert_eq!(actual_path, path);
            calls += 1;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(actual_path)?;
            file.write_all(b"foreign creator exact bytes")?;
            file.sync_all()?;
            creator = Some(file);
            Ok(())
        });
        // Release the actual owned creator before any potentially failing
        // assertion, including a skipped-callback or initializer error.
        drop(creator);
        result.expect("concurrent creator preserved");
        assert_eq!(calls, 1);
        assert_eq!(
            std::fs::read(&path).expect("foreign bytes"),
            b"foreign creator exact bytes"
        );
    }

    #[test]
    fn starter_config_readonly_existing_file_still_loads_without_mutation() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let path = directory.path().join("config.json");
        let bytes = br#"{"service_name":"SyntheticReadOnly"}"#;
        std::fs::write(&path, bytes).expect("readonly config fixture");
        let original_permissions = std::fs::metadata(&path).expect("metadata").permissions();
        let mut readonly_permissions = original_permissions.clone();
        readonly_permissions.set_readonly(true);
        std::fs::set_permissions(&path, readonly_permissions).expect("set readonly fixture");
        let initialized = maybe_write_default_config(&path);
        let config = load_settings_config_at(&path);
        let after = std::fs::read(&path).expect("read readonly config");
        std::fs::set_permissions(&path, original_permissions).expect("restore owned permissions");
        initialized.expect("existing readonly config does not need a writer");
        assert_eq!(config.service_name.as_deref(), Some("SyntheticReadOnly"));
        assert!(config.configuration_error.is_none());
        assert_eq!(after, bytes);
    }

    #[test]
    fn starter_config_parent_failure_is_propagated_and_blocks_settings() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let parent = directory.path().join("foreign-parent-file");
        std::fs::write(&parent, b"foreign parent bytes").expect("foreign parent");
        let path = parent.join("config.json");
        let error = maybe_write_default_config(&path).expect_err("parent is a file");
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);
        let config = load_settings_config_at(&path);
        assert!(
            config
                .configuration_error
                .as_deref()
                .is_some_and(|error| error.contains("could not be initialized"))
        );
        assert!(
            resolve_candidate_tiers(&config, None, None, None)
                .1
                .is_some()
        );
        assert_eq!(
            std::fs::read(&parent).expect("preserved parent"),
            b"foreign parent bytes"
        );
        assert!(!path.exists());
    }

    #[test]
    fn starter_config_preopen_io_failure_never_creates_target() {
        let directory = tempfile::tempdir().expect("temporary config directory");
        let path = directory.path().join("config.json");
        let mut calls = 0;
        let error = write_default_config_before_open(&path, |actual_path| {
            assert_eq!(actual_path, path);
            calls += 1;
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .expect_err("external I/O boundary failure");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(calls, 1);
        assert!(!path.exists());
    }

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
        with_server_url_env(None, None, || {
            let s = Settings::from_env();
            assert!(s.redis_url.starts_with("redis://"));
            assert!(!s.stream_key.is_empty());
            assert!(!s.channel_key.is_empty());
            assert!(s.stream_maxlen > 0);
        });
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
        let mut s = Settings::for_test();
        s.redis_url = "redis://remote-host:6380/0".to_owned();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_non_localhost_database() {
        let mut s = Settings::for_test();
        s.database_url = Some("postgresql://postgres@remote:5432/db".to_owned());
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_non_localhost_server_host() {
        let mut s = Settings::for_test();
        s.server_host = "0.0.0.0".to_owned();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_remote_bind_without_auth_token() {
        let mut s = Settings::for_test();
        s.server_host = "0.0.0.0".to_owned();
        s.allow_remote = true;
        s.auth_token = None;
        // allow_remote alone is not enough — an unauthenticated remote bind
        // must be refused.
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_accepts_remote_bind_with_allow_and_token() {
        let mut s = Settings::for_test();
        s.redis_url = "redis://localhost:6380/0".to_owned();
        s.database_url = Some("postgresql://postgres@localhost:5432/db".to_owned());
        s.server_host = "0.0.0.0".to_owned();
        s.allow_remote = true;
        s.auth_token = Some("secret-token".to_owned());
        assert!(s.validate().is_ok());
    }

    #[test]
    fn validate_accepts_localhost_variants() {
        let mut s = Settings::for_test();
        s.redis_url = "redis://localhost:6380/0".to_owned();
        s.database_url = Some("postgresql://postgres@localhost:5432/db".to_owned());
        s.server_host = "localhost".to_owned();
        assert!(s.validate().is_ok());
    }

    #[test]
    fn validate_accepts_127_0_0_1() {
        let mut s = Settings::for_test();
        s.redis_url = "redis://127.0.0.1:6380/0".to_owned();
        s.server_host = "127.0.0.1".to_owned();
        assert!(s.validate().is_ok());
    }

    #[test]
    fn validate_accepts_bracketed_ipv6_backend_urls() {
        let mut s = Settings::for_test();
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
        let mut s = Settings::for_test();
        s.database_url = None;
        assert!(s.validate().is_ok());
    }

    // -----------------------------------------------------------------------
    // Settings::validate — identifier checks
    // -----------------------------------------------------------------------

    #[test]
    fn validate_rejects_empty_stream_key() {
        let mut s = Settings::for_test();
        s.stream_key = String::new();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_stream_key_with_spaces() {
        let mut s = Settings::for_test();
        s.stream_key = "bad stream key".to_owned();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_table_name() {
        let mut s = Settings::for_test();
        s.message_table = String::new();
        assert!(s.validate().is_err());
    }

    #[test]
    fn validate_accepts_dotted_table_name() {
        let s = Settings::for_test();
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
        with_server_url_env(None, None, || {
            let s = Settings::from_env();
            assert_eq!(s.server_url, None);
            assert!(s.server_urls.is_empty());
            assert!(s.hub_candidates.is_empty());
            assert_eq!(s.hub_config_error, None);
        });
    }

    #[test]
    fn config_file_deserializes_machine_safe() {
        let json = r#"{"machine_safe": true}"#;
        let cfg: ConfigFile = serde_json::from_str(json).expect("valid JSON");
        assert_eq!(cfg.machine_safe, Some(true));
    }

    #[test]
    fn log_non_fatal_warnings_disabled_when_machine_safe_enabled() {
        let mut s = Settings::for_test();
        s.machine_safe = true;
        assert!(!s.log_non_fatal_warnings());
    }

    // -----------------------------------------------------------------------
    // server_urls — ordered hub-candidate list (#78)
    // -----------------------------------------------------------------------

    /// Serializes access to the `AGENT_BUS_SERVER_URL{,S}` env vars across
    /// this module's tests: `cargo test` runs tests in this file on multiple
    /// threads by default, and these vars are process-global.
    fn with_server_url_env<T>(urls: Option<&str>, url: Option<&str>, f: impl FnOnce() -> T) -> T {
        struct RestoreEnvironment(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for RestoreEnvironment {
            fn drop(&mut self) {
                // SAFETY: the owning test retains LOCK until this guard drops.
                unsafe {
                    for (key, value) in &self.0 {
                        match value {
                            Some(value) => std::env::set_var(key, value),
                            None => std::env::remove_var(key),
                        }
                    }
                }
            }
        }
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Isolate from the host's real config and every candidate env tier.
        let _restore = RestoreEnvironment(
            [
                "AGENT_BUS_CONFIG",
                "AGENT_BUS_SERVER_CANDIDATES",
                "AGENT_BUS_SERVER_URLS",
                "AGENT_BUS_SERVER_URL",
            ]
            .into_iter()
            .map(|key| (key, std::env::var_os(key)))
            .collect(),
        );
        let directory = tempfile::tempdir().expect("isolated settings fixture");
        let missing_config = directory.path().join("config.json");
        // SAFETY: serialized by LOCK; no other thread touches these vars
        // while the guard is held (every test in this module goes through
        // this helper or leaves them untouched).
        unsafe {
            std::env::set_var("AGENT_BUS_CONFIG", &missing_config);
            std::env::remove_var("AGENT_BUS_SERVER_CANDIDATES");
            match urls {
                Some(v) => std::env::set_var("AGENT_BUS_SERVER_URLS", v),
                None => std::env::remove_var("AGENT_BUS_SERVER_URLS"),
            }
            match url {
                Some(v) => std::env::set_var("AGENT_BUS_SERVER_URL", v),
                None => std::env::remove_var("AGENT_BUS_SERVER_URL"),
            }
        }
        f()
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
                server_urls: Some(serde_json::json!([
                    "http://cfg-a:8400".to_owned(),
                    "http://cfg-b:8400".to_owned(),
                ])),
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
            Some(serde_json::json!(["http://a:8400", "http://b:8400"]))
        );
    }

    #[test]
    fn object_candidates_preserve_token_sources_and_roles() {
        with_server_url_env(None, None, || {
            let cfg: ConfigFile = serde_json::from_value(serde_json::json!({
                "server_urls": ["http://hub.lan:8400", {"url":"https://agentbus.example.com", "token_env":"CLOUD_AGENT_TOKEN", "role":"cloud"}]
            })).expect("valid object configuration");
            let (candidates, error) = resolve_candidate_settings(&cfg);
            assert_eq!(error, None);
            assert_eq!(candidates.len(), 2);
            assert_eq!(candidates[1].role, crate::hub_candidates::HubRole::Cloud);
            assert_eq!(
                candidates[1].auth,
                crate::hub_candidates::CandidateAuth::TokenEnv("CLOUD_AGENT_TOKEN".to_owned())
            );
        });
    }

    #[test]
    fn explicit_json_tier_cannot_accidentally_create_local_mode() {
        let cfg = ConfigFile {
            server_url: Some("http://hub.lan:8400".to_owned()),
            ..ConfigFile::default()
        };
        for raw in ["[]", "[\"\", \" \"]", "not-json"] {
            let (candidates, error) = resolve_candidate_tiers(&cfg, Some(raw), None, None);
            assert!(candidates.is_empty());
            assert!(
                error.is_some(),
                "{raw} must block local I/O rather than override the configured hub"
            );
        }
    }

    #[test]
    fn json_tier_preserves_its_cloud_credential_over_legacy_overrides() {
        let (candidates, error) = resolve_candidate_tiers(
            &ConfigFile::default(),
            Some(
                r#"[{"url":"https://cloud.example.com","role":"cloud","token_env":"CLOUD_TOKEN"}]"#,
            ),
            Some("http://legacy.lan:8400"),
            Some("http://single.lan:8400"),
        );
        assert_eq!(error, None);
        assert_eq!(candidates[0].url, "https://cloud.example.com");
        assert_eq!(
            candidates[0].auth,
            crate::hub_candidates::CandidateAuth::TokenEnv("CLOUD_TOKEN".to_owned())
        );
        assert_eq!(candidates[0].role, crate::hub_candidates::HubRole::Cloud);
    }

    #[test]
    fn candidate_typo_and_conflicting_duplicates_fail_closed() {
        with_server_url_env(None, None, || {
            for raw in [
                serde_json::json!([{"url":"https://cloud.example.com","token_en":"CLOUD_TOKEN"}]),
                serde_json::json!([{"url":"http://hub.lan:8400","role":"fallback"},{"url":"http://hub.lan:8400","role":"authoritative"}]),
            ] {
                let cfg = ConfigFile {
                    server_urls: Some(raw),
                    ..ConfigFile::default()
                };
                let (candidates, error) = resolve_candidate_settings(&cfg);
                assert!(candidates.is_empty());
                assert!(error.is_some());
            }
        });
    }

    #[test]
    fn configuration_error_blocks_local_backend() {
        let mut settings = Settings::for_test();
        settings.hub_config_error = Some("invalid candidate configuration".to_owned());
        settings.server_urls.clear();
        settings.hub_candidates.clear();
        assert!(
            settings
                .validate()
                .expect_err("invalid configuration must never become local mode")
                .to_string()
                .contains("invalid candidate")
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
                server_urls: Some(serde_json::json!([
                    "http://cfg-a:8400".to_owned(),
                    "http://cfg-b:8400".to_owned(),
                    "http://cfg-a:8400".to_owned(),
                ])),
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
                server_urls: Some(serde_json::json!([
                    "http://cfg-a:8400".to_owned(),
                    "http://cfg-b:8400".to_owned(),
                ])),
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
