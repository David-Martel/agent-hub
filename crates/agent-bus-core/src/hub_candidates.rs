//! Per-candidate hub roles and credentials (agent-hub#79).
//!
//! One global `auth_token` used to be sent as a Bearer token to EVERY hub
//! candidate. That is unsafe the moment a candidate lives in a different
//! trust domain: the off-site Cloudflare tier uses its own credential space
//! and must never receive the on-site hub's token. This module models each
//! candidate's role and credential source explicitly, and enforces a
//! fail-closed guard: the global token is never sent to a `cloud` candidate,
//! nor to a public HTTP or HTTPS candidate. Unknown request URLs may use
//! the global token only for the recognized private service-admin endpoints.
//!
//! Everything here is pure or takes an injectable [`CredentialEnv`], so the
//! parsing, the guard and the token selection are unit-testable without
//! touching the process environment or the file system.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::{Host, Url};

/// What a hub candidate is allowed to do.
///
/// Only an [`HubRole::Authoritative`] candidate may grant, renew, release or
/// resolve exclusive claims. A [`HubRole::Cloud`] candidate is never
/// authoritative and never receives the global token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HubRole {
    /// The single claims authority (index 0 unless configured otherwise).
    Authoritative,
    /// A reachable lower-priority hub (every non-first candidate by default).
    Fallback,
    /// The off-site cloud tier: its own credential space, never authoritative.
    Cloud,
}

impl HubRole {
    /// Lowercase wire/config name (`authoritative`, `fallback`, `cloud`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authoritative => "authoritative",
            Self::Fallback => "fallback",
            Self::Cloud => "cloud",
        }
    }

    /// Parse a config value, ASCII-case-insensitively. `None` if unknown.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "authoritative" => Some(Self::Authoritative),
            "fallback" => Some(Self::Fallback),
            "cloud" => Some(Self::Cloud),
            _ => None,
        }
    }
}

/// Where a candidate's bearer token comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandidateAuth {
    /// Legacy string entry: the global `auth_token`, subject to the guard.
    Global,
    /// Read the token from this file at use time (leading `~` expanded).
    TokenFile(String),
    /// Read the token from this environment variable at use time.
    TokenEnv(String),
}

/// The explicit, parsed configuration of one candidate. Fields left `None`
/// or [`CandidateAuth::Global`] mean "use the default".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateSpec {
    /// The hub base URL exactly as configured (trimmed).
    pub url: String,
    /// An explicitly configured role, if any.
    pub role: Option<HubRole>,
    /// The credential source.
    pub auth: CandidateAuth,
}

/// A fully resolved candidate: URL, effective role and credential source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubCandidate {
    /// The hub base URL.
    pub url: String,
    /// The effective role (explicit, or defaulted by position).
    pub role: HubRole,
    /// The credential source.
    pub auth: CandidateAuth,
}

/// A bearer token. `Debug` never prints the value.
#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken(String);

impl BearerToken {
    /// Wrap a token value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw token. Only call this when building a request header.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BearerToken(<redacted>)")
    }
}

/// Why a candidate cannot be used. Never contains a token value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkipReason(pub String);

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Environment access for credential resolution, injectable for tests.
pub trait CredentialEnv {
    /// Read an environment variable.
    fn var(&self, name: &str) -> Option<String>;
    /// Read a whole file as UTF-8 text.
    ///
    /// # Errors
    /// Returns the underlying I/O error.
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    /// The user's home directory, for expanding a leading `~`.
    fn home_dir(&self) -> Option<PathBuf>;
}

/// The real process environment and file system.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEnv;

impl CredentialEnv for SystemEnv {
    fn var(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn home_dir(&self) -> Option<PathBuf> {
        let name = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        self.var(name)
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
    }
}

/// Parse the raw `server_urls` / `AGENT_BUS_SERVER_CANDIDATES` JSON value
/// into candidate specs. Entries are a URL string (legacy) or an object
/// `{"url", "token_file"?, "token_env"?, "role"?}`; blank string entries are
/// dropped (legacy behaviour).
///
/// # Errors
/// A human-readable message for any invalid shape: a non-array top level, an
/// entry that is neither string nor object, a missing/blank `url`, an unknown
/// key (a typo must never silently fall back to the global token), both
/// `token_file` and `token_env`, a blank token reference, or an unknown role.
pub fn parse_candidate_entries(value: &Value) -> Result<Vec<CandidateSpec>, String> {
    let entries = value
        .as_array()
        .ok_or("candidate configuration must be an array")?;
    let mut specs = Vec::with_capacity(entries.len());
    let mut seen = BTreeMap::<String, CandidateSpec>::new();
    for (index, entry) in entries.iter().enumerate() {
        let spec = match entry {
            Value::String(raw) if raw.trim().is_empty() => continue,
            Value::String(raw) => CandidateSpec {
                url: raw.trim().to_owned(),
                role: None,
                auth: CandidateAuth::Global,
            },
            Value::Object(fields) => parse_candidate_object(fields)
                .map_err(|error| format!("candidate entry {}: {error}", index + 1))?,
            _ => {
                return Err(format!(
                    "candidate entry {} must be a URL string or object",
                    index + 1
                ));
            }
        };
        let parsed = parse_hub_url(&spec.url, false)
            .map_err(|error| format!("candidate entry {} url: {error}", index + 1))?;
        let key = base_key(&parsed);
        if let Some(previous) = seen.get(&key) {
            if previous.role != spec.role || previous.auth != spec.auth {
                return Err(format!(
                    "candidate entry {} conflicts with a duplicate URL",
                    index + 1
                ));
            }
            continue;
        }
        seen.insert(key, spec.clone());
        specs.push(spec);
    }
    Ok(specs)
}

fn parse_candidate_object(
    fields: &serde_json::Map<String, Value>,
) -> Result<CandidateSpec, String> {
    for name in fields.keys() {
        if !matches!(name.as_str(), "url" | "role" | "token_file" | "token_env") {
            return Err(format!("unknown candidate key {name:?}"));
        }
    }
    let url = text_field(fields, "url")?.ok_or("url is required")?;
    let role = text_field(fields, "role")?
        .map(|raw| {
            HubRole::parse(raw).ok_or_else(|| {
                "unknown role; expected authoritative, fallback, or cloud".to_owned()
            })
        })
        .transpose()?;
    let file = text_field(fields, "token_file")?;
    let variable = text_field(fields, "token_env")?;
    let auth = match (file, variable) {
        (Some(_), Some(_)) => {
            return Err("token_file and token_env are mutually exclusive".to_owned());
        }
        (Some(path), None) if !path.chars().any(char::is_control) => {
            CandidateAuth::TokenFile(path.to_owned())
        }
        (Some(_), None) => return Err("token_file contains invalid characters".to_owned()),
        (None, Some(name)) if valid_env_name(name) => CandidateAuth::TokenEnv(name.to_owned()),
        (None, Some(_)) => return Err("token_env must be an environment variable name".to_owned()),
        (None, None) => CandidateAuth::Global,
    };
    Ok(CandidateSpec {
        url: url.to_owned(),
        role,
        auth,
    })
}

fn text_field<'a>(
    fields: &'a serde_json::Map<String, Value>,
    name: &str,
) -> Result<Option<&'a str>, String> {
    let Some(value) = fields.get(name) else {
        return Ok(None);
    };
    let text = value
        .as_str()
        .ok_or_else(|| format!("{name} must be a string"))?
        .trim();
    if text.is_empty() {
        return Err(format!("{name} must not be blank"));
    }
    Ok(Some(text))
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Parse a base URL without exposing userinfo or query values in diagnostics.
///
/// # Errors
/// Rejects malformed/non-HTTP URLs, userinfo, queries and fragments.
pub(crate) fn validate_hub_base_url(value: &str) -> Result<(), String> {
    parse_hub_url(value, false).map(|_| ())
}

fn parse_hub_url(value: &str, allow_query: bool) -> Result<Url, String> {
    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err("URL contains whitespace or control characters".to_owned());
    }
    let parsed = Url::parse(value).map_err(|_error| "invalid absolute HTTP URL".to_owned())?;
    if value
        .get(..parsed.scheme().len() + 3)
        .is_none_or(|prefix| prefix.to_ascii_lowercase() != format!("{}://", parsed.scheme()))
        || value.contains('\\')
    {
        return Err("URL must be an absolute HTTP URL without backslashes".to_owned());
    }
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err("URL must use http or https and include a host".to_owned());
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("URL userinfo is not allowed".to_owned());
    }
    if parsed.fragment().is_some() || (!allow_query && parsed.query().is_some()) {
        return Err("URL fragment or base query is not allowed".to_owned());
    }
    Ok(parsed)
}

fn base_key(url: &Url) -> String {
    url.as_str().trim_end_matches('/').to_owned()
}

/// Parse a JSON text with [`parse_candidate_entries`].
///
/// # Errors
/// A message when the text is not valid JSON, or any error from
/// [`parse_candidate_entries`].
pub fn parse_candidate_json(raw: &str) -> Result<Vec<CandidateSpec>, String> {
    let value: UniqueJson =
        serde_json::from_str(raw).map_err(|_error| "invalid candidate JSON".to_owned())?;
    parse_candidate_entries(&value.0)
}

/// Preserve JSON's value shape while rejecting duplicate object keys.
#[derive(Debug)]
struct UniqueJson(Value);

/// Deserialize optional candidate configuration without duplicate object keys.
pub(crate) fn deserialize_candidate_value<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Value>, D::Error> {
    Option::<UniqueJson>::deserialize(deserializer).map(|value| value.map(|value| value.0))
}

impl<'de> Deserialize<'de> for UniqueJson {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueJson;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("JSON with unique object keys")
            }
            fn visit_bool<E: serde::de::Error>(self, value: bool) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Bool(value)))
            }
            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::from(value)))
            }
            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::from(value)))
            }
            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(value)
                    .map(|number| UniqueJson(Value::Number(number)))
                    .ok_or_else(|| E::custom("invalid JSON number"))
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(value.to_owned())))
            }
            fn visit_string<E: serde::de::Error>(self, value: String) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::String(value)))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueJson(Value::Null))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut sequence: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element::<UniqueJson>()? {
                    values.push(value.0);
                }
                Ok(UniqueJson(Value::Array(values)))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut map: A,
            ) -> Result<Self::Value, A::Error> {
                let mut values = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(serde::de::Error::custom("duplicate JSON object key"));
                    }
                    values.insert(key, map.next_value::<UniqueJson>()?.0);
                }
                Ok(UniqueJson(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// Combine the ordered URL list with the explicit per-URL specs into
/// resolved candidates. Role defaults: index 0 is authoritative, every other
/// candidate is a fallback, unless the spec names a role.
#[must_use]
pub fn build_candidates(
    urls: &[String],
    specs: &BTreeMap<String, CandidateSpec>,
) -> Vec<HubCandidate> {
    urls.iter()
        .enumerate()
        .map(|(index, url)| {
            let spec = specs.get(url).or_else(|| {
                let parsed = parse_hub_url(url, false).ok()?;
                specs.values().find(|spec| {
                    parse_hub_url(&spec.url, false)
                        .is_ok_and(|other| base_key(&parsed) == base_key(&other))
                })
            });
            HubCandidate {
                url: url.clone(),
                role: spec.and_then(|spec| spec.role).unwrap_or(if index == 0 {
                    HubRole::Authoritative
                } else {
                    HubRole::Fallback
                }),
                auth: spec.map_or(CandidateAuth::Global, |spec| spec.auth.clone()),
            }
        })
        .collect()
}

/// Reject configurations with more than one authoritative candidate: exactly
/// one hub may ever grant a claim.
///
/// # Errors
/// A message naming every authoritative candidate when there is more than one.
pub fn validate_candidates(candidates: &[HubCandidate]) -> Result<(), String> {
    let mut urls = std::collections::BTreeSet::new();
    for candidate in candidates {
        let url = parse_hub_url(&candidate.url, false)?;
        if !urls.insert(base_key(&url)) {
            return Err("duplicate canonical hub URL".to_owned());
        }
    }
    let authorities: Vec<&str> = candidates
        .iter()
        .filter(|candidate| candidate.role == HubRole::Authoritative)
        .map(|candidate| candidate.url.as_str())
        .collect();
    if authorities.len() > 1 {
        return Err(format!(
            "multiple authoritative candidates: {}",
            authorities.join(", ")
        ));
    }
    Ok(())
}

/// `true` when `url` is `https` and its host is a public DNS name or public
/// IP literal, i.e. anything that is not localhost, a loopback / RFC1918 /
/// link-local / CGNAT / ULA address, a single-label name, or a
/// `*.lan` / `*.local` / `*.internal` / `*.home.arpa` name. Fails closed: an
/// `https` URL whose host cannot be determined counts as public.
#[must_use]
pub fn is_public_https_target(url: &str) -> bool {
    let Ok(parsed) = Url::parse(url) else {
        return url.trim().to_ascii_lowercase().starts_with("https:");
    };
    if parsed.scheme() != "https" {
        return false;
    }
    public_host(&parsed)
}

fn public_host(parsed: &Url) -> bool {
    match parsed.host() {
        Some(Host::Ipv4(address)) => !private_ipv4(address),
        Some(Host::Ipv6(address)) => {
            let first = address.segments()[0];
            !(address.is_loopback()
                || address.is_unspecified()
                || first & 0xfe00 == 0xfc00
                || first & 0xffc0 == 0xfe80
                || address.to_ipv4_mapped().is_some_and(private_ipv4))
        }
        Some(Host::Domain(host)) => {
            let host = host.trim_end_matches('.').to_ascii_lowercase();
            host.contains('.')
                && host != "home.arpa"
                && ![".localhost", ".lan", ".local", ".internal", ".home.arpa"]
                    .iter()
                    .any(|suffix| host.ends_with(suffix))
        }
        None => true,
    }
}

fn private_ipv4(address: std::net::Ipv4Addr) -> bool {
    let octets = address.octets();
    address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_unspecified()
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
}

fn token_value(raw: &str) -> Result<BearerToken, SkipReason> {
    let token = raw.trim();
    let body = token.trim_end_matches('=');
    if body.is_empty()
        || !body
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-._~+/".contains(&byte))
    {
        return Err(SkipReason("credential is empty or malformed".to_owned()));
    }
    Ok(BearerToken::new(token))
}

fn expanded_token_path(value: &str, env: &dyn CredentialEnv) -> Result<PathBuf, SkipReason> {
    if value == "~" || value.starts_with("~/") || value.starts_with("~\\") {
        let home = env
            .home_dir()
            .ok_or_else(|| SkipReason("cannot expand ~ without a home directory".to_owned()))?;
        return Ok(if value == "~" {
            home
        } else {
            home.join(&value[2..])
        });
    }
    Ok(PathBuf::from(value))
}

impl HubCandidate {
    /// The bearer token to send to this candidate, resolved now (a token
    /// file is read at call time).
    ///
    /// `Ok(None)` means "send no Authorization header" (a legacy string
    /// entry with no global token configured).
    ///
    /// # Errors
    /// [`SkipReason`] when the candidate must not be contacted: the guard
    /// forbids the global token, or the candidate's own token is missing,
    /// unreadable, empty or malformed. The global token is NEVER a fallback
    /// for a candidate that names its own token source.
    pub fn authorization(
        &self,
        global: Option<&str>,
        env: &dyn CredentialEnv,
    ) -> Result<Option<BearerToken>, SkipReason> {
        validate_hub_base_url(&self.url).map_err(SkipReason)?;
        match &self.auth {
            CandidateAuth::Global => {
                if self.role == HubRole::Cloud
                    || public_host(&parse_hub_url(&self.url, false).map_err(SkipReason)?)
                {
                    return Err(SkipReason("candidate requires its own token_file or token_env; global credentials are forbidden".to_owned()));
                }
                global
                    .filter(|raw| !raw.trim().is_empty())
                    .map(token_value)
                    .transpose()
            }
            CandidateAuth::TokenFile(path) => {
                if path.trim().is_empty() || path.chars().any(char::is_control) {
                    return Err(SkipReason("invalid token_file reference".to_owned()));
                }
                let expanded = expanded_token_path(path, env)?;
                let raw = env.read_to_string(&expanded).map_err(|error| {
                    SkipReason(format!(
                        "cannot read token_file {path:?}: {:?}",
                        error.kind()
                    ))
                })?;
                token_value(&raw).map(Some)
            }
            CandidateAuth::TokenEnv(name) => {
                if !valid_env_name(name) {
                    return Err(SkipReason("invalid token_env reference".to_owned()));
                }
                let raw = env
                    .var(name)
                    .ok_or_else(|| SkipReason(format!("token_env {name} is not set")))?;
                token_value(&raw)
                    .map(Some)
                    .map_err(|_error| SkipReason(format!("token_env {name} is empty or malformed")))
            }
        }
    }
}

/// All candidates plus the global token: the single place that decides which
/// token (if any) a request to a given URL may carry.
#[derive(Clone)]
pub struct HubAuth {
    global: Option<String>,
    candidates: Vec<HubCandidate>,
}

impl fmt::Debug for HubAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubAuth")
            .field("global", &self.global.as_ref().map(|_| "<redacted>"))
            .field("candidates", &self.candidates)
            .finish()
    }
}

impl HubAuth {
    /// Build from the global token and the resolved candidates.
    #[must_use]
    pub const fn new(global: Option<String>, candidates: Vec<HubCandidate>) -> Self {
        Self { global, candidates }
    }

    /// The resolved candidates, in priority order.
    #[must_use]
    pub fn candidates(&self) -> &[HubCandidate] {
        &self.candidates
    }

    /// Token for `candidate`; see [`HubCandidate::authorization`].
    ///
    /// # Errors
    /// [`SkipReason`] when the candidate must not be contacted.
    pub fn credential_for(
        &self,
        candidate: &HubCandidate,
        env: &dyn CredentialEnv,
    ) -> Result<Option<BearerToken>, SkipReason> {
        candidate.authorization(self.global.as_deref(), env)
    }

    /// Token for a request URL: the candidate whose base URL the request
    /// falls under (longest match). Unmatched URLs may carry the global
    /// token only at private `/admin/service`, `/admin/service/control`,
    /// or `/health` endpoints.
    ///
    /// # Errors
    /// [`SkipReason`] when the request must not carry any token.
    pub fn credential_for_url(
        &self,
        url: &str,
        env: &dyn CredentialEnv,
    ) -> Result<Option<BearerToken>, SkipReason> {
        let request = parse_hub_url(url, true).map_err(SkipReason)?;
        let mut matching = None;
        let mut longest = 0;
        for candidate in &self.candidates {
            let base = parse_hub_url(&candidate.url, false).map_err(SkipReason)?;
            let path = base.path().trim_end_matches('/');
            let request_path = request.path();
            if base.origin() == request.origin()
                && (path.is_empty()
                    || request_path == path
                    || request_path
                        .strip_prefix(path)
                        .is_some_and(|suffix| suffix.starts_with('/')))
            {
                if matching.is_some() && path.len() == longest {
                    return Err(SkipReason("ambiguous canonical hub URL match".to_owned()));
                }
                if matching.is_none() || path.len() > longest {
                    matching = Some(candidate);
                    longest = path.len();
                }
            }
        }
        if let Some(candidate) = matching {
            return self.credential_for(candidate, env);
        }
        if public_host(&request)
            || !matches!(
                request.path(),
                "/admin/service" | "/admin/service/control" | "/health"
            )
        {
            return Err(SkipReason(
                "unmatched request URL is not a private service endpoint".to_owned(),
            ));
        }
        HubCandidate {
            url: format!(
                "{}{}",
                request.origin().ascii_serialization(),
                request.path()
            ),
            role: HubRole::Fallback,
            auth: CandidateAuth::Global,
        }
        .authorization(self.global.as_deref(), env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    const GLOBAL: &str = "global-sekret-token";

    #[test]
    fn duplicate_fields_and_conflicting_canonical_candidates_are_rejected() {
        #[derive(Debug, Deserialize)]
        struct Config {
            #[serde(default, deserialize_with = "deserialize_candidate_value")]
            server_urls: Option<Value>,
        }
        for raw in [
            r#"[{"url":"http://hub.lan","url":"http://other.lan"}]"#,
            r#"[{"url":"http://hub.lan","token_env":"A","token_env":"B"}]"#,
            r#"[{"url":"http://HUB.lan:80/","token_env":"A"},{"url":"http://hub.lan","token_env":"B"}]"#,
        ] {
            assert!(parse_candidate_json(raw).is_err(), "{raw}");
        }
        assert!(
            serde_json::from_str::<Config>(
                r#"{"server_urls":[{"url":"http://hub.lan","role":"fallback","role":"cloud"}]}"#
            )
            .is_err()
        );
        let config: Config =
            serde_json::from_str(r#"{"server_urls":["http://hub.lan"]}"#).expect("valid config");
        assert_eq!(
            parse_candidate_entries(config.server_urls.as_ref().expect("urls")).expect("parse")[0]
                .url,
            "http://hub.lan"
        );
        assert!(
            serde_json::from_str::<Config>("{}")
                .expect("absent")
                .server_urls
                .is_none()
        );
    }

    #[test]
    fn unsafe_urls_and_role_diagnostics_never_echo_supplied_secrets() {
        for url in [
            "http://user:secret@hub.lan",
            "https://hub.lan/?token=secret",
            "http://hub.lan/#secret",
            "file:///secret",
            "http:hub.lan",
            "http://hub.lan\\secret",
        ] {
            let raw = serde_json::json!([{ "url": url }]);
            let error = parse_candidate_entries(&raw).expect_err("unsafe URL");
            assert!(!error.contains("secret"), "{error}");
        }
        let error = parse_candidate_json(r#"[{"url":"http://hub.lan","role":"secret-role"}]"#)
            .expect_err("unknown role");
        assert!(!error.contains("secret-role"));
    }

    #[test]
    fn public_http_and_unrecognized_private_endpoints_cannot_receive_global_credentials() {
        let env = FakeEnv::default();
        let auth = HubAuth::new(Some(GLOBAL.to_owned()), Vec::new());
        for url in [
            "http://example.com/health",
            "https://example.com/health",
            "http://localhost:8400/messages",
            "http://localhost:8400/admin/service/extra",
        ] {
            assert!(auth.credential_for_url(url, &env).is_err(), "{url}");
        }
        for url in [
            "http://localhost:8400/health",
            "http://hub.lan/admin/service",
            "https://10.0.0.1/admin/service/control",
        ] {
            assert_eq!(
                auth.credential_for_url(url, &env)
                    .expect("private endpoint")
                    .expect("token")
                    .expose(),
                GLOBAL
            );
        }
        assert!(
            candidate(
                "http://example.com",
                HubRole::Fallback,
                CandidateAuth::Global
            )
            .authorization(Some(GLOBAL), &env)
            .is_err()
        );
    }

    #[test]
    fn configured_path_prefix_requires_a_segment_boundary_and_parsed_origin() {
        let mut env = FakeEnv::default();
        env.vars.insert("OWN".to_owned(), "own-token".to_owned());
        let auth = HubAuth::new(
            Some(GLOBAL.to_owned()),
            vec![candidate(
                "http://HUB.lan:80/sub/",
                HubRole::Fallback,
                CandidateAuth::TokenEnv("OWN".to_owned()),
            )],
        );
        assert_eq!(
            auth.credential_for_url("http://hub.lan/sub/health?x=1", &env)
                .expect("match")
                .expect("own token")
                .expose(),
            "own-token"
        );
        for url in [
            "http://hub.lan/submarine",
            "http://hub.lan:81/sub/health",
            "http://hub.lan.evil/sub/health",
        ] {
            assert!(auth.credential_for_url(url, &env).is_err(), "{url}");
        }
    }

    /// In-memory environment recording every file path it was asked to read.
    #[derive(Default)]
    struct FakeEnv {
        vars: HashMap<String, String>,
        files: HashMap<PathBuf, String>,
        home: Option<PathBuf>,
        reads: RefCell<Vec<PathBuf>>,
    }

    impl CredentialEnv for FakeEnv {
        fn var(&self, name: &str) -> Option<String> {
            self.vars.get(name).cloned()
        }

        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.reads.borrow_mut().push(path.to_path_buf());
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        }

        fn home_dir(&self) -> Option<PathBuf> {
            self.home.clone()
        }
    }

    fn candidate(url: &str, role: HubRole, auth: CandidateAuth) -> HubCandidate {
        HubCandidate {
            url: url.to_owned(),
            role,
            auth,
        }
    }

    fn string_candidate(url: &str) -> HubCandidate {
        candidate(url, HubRole::Fallback, CandidateAuth::Global)
    }

    // ---------------------------------------------------------------
    // Parsing
    // ---------------------------------------------------------------

    fn parse(json: &str) -> Result<Vec<CandidateSpec>, String> {
        parse_candidate_json(json)
    }

    #[test]
    fn legacy_string_entries_are_trimmed_and_blanks_dropped() {
        let specs = parse(r#"["http://a:8400", "  http://b:8400  ", "", "   "]"#).expect("valid");
        assert_eq!(
            specs,
            vec![
                CandidateSpec {
                    url: "http://a:8400".to_owned(),
                    role: None,
                    auth: CandidateAuth::Global,
                },
                CandidateSpec {
                    url: "http://b:8400".to_owned(),
                    role: None,
                    auth: CandidateAuth::Global,
                },
            ]
        );
    }

    #[test]
    fn object_entry_with_every_field_parses() {
        let specs = parse(
            r#"[{"url": "https://agentbus.dtmventures.com", "role": "cloud",
                 "token_file": "~/.config/agentbus-cloud/agent-x.token"}]"#,
        )
        .expect("valid");
        assert_eq!(
            specs,
            vec![CandidateSpec {
                url: "https://agentbus.dtmventures.com".to_owned(),
                role: Some(HubRole::Cloud),
                auth: CandidateAuth::TokenFile("~/.config/agentbus-cloud/agent-x.token".to_owned()),
            }]
        );
    }

    #[test]
    fn object_entry_with_token_env_parses_and_url_only_object_is_global() {
        let specs = parse(
            r#"[{"url": "http://a:8400", "token_env": "HUB_A_TOKEN"}, {"url": "http://b:8400"}]"#,
        )
        .expect("valid");
        assert_eq!(
            specs[0].auth,
            CandidateAuth::TokenEnv("HUB_A_TOKEN".to_owned())
        );
        assert_eq!(specs[0].role, None);
        assert_eq!(specs[1].auth, CandidateAuth::Global);
    }

    #[test]
    fn mixed_string_and_object_entries_keep_order() {
        let specs = parse(r#"["http://a:8400", {"url": "http://b:8400", "role": "fallback"}]"#)
            .expect("valid");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].url, "http://a:8400");
        assert_eq!(specs[1].url, "http://b:8400");
        assert_eq!(specs[1].role, Some(HubRole::Fallback));
    }

    #[test]
    fn role_is_case_insensitive() {
        let specs = parse(r#"[{"url": "http://a:8400", "role": "CLOUD", "token_env": "T"}]"#)
            .expect("valid");
        assert_eq!(specs[0].role, Some(HubRole::Cloud));
    }

    #[test]
    fn non_array_top_level_is_rejected() {
        for raw in [
            r#"{"url": "http://a:8400"}"#,
            r#""http://a:8400""#,
            "42",
            "null",
        ] {
            let err = parse(raw).expect_err(raw);
            assert!(err.contains("array"), "{raw}: {err}");
        }
    }

    #[test]
    fn invalid_json_is_rejected_with_a_clear_error() {
        let err = parse("[not json").expect_err("invalid json");
        assert!(err.to_lowercase().contains("json"), "{err}");
    }

    #[test]
    fn entry_of_the_wrong_type_is_rejected_with_its_position() {
        for raw in [
            r#"["http://a:8400", 7]"#,
            r#"["http://a:8400", null]"#,
            r#"["http://a:8400", true]"#,
            r#"["http://a:8400", ["http://b:8400"]]"#,
        ] {
            let err = parse(raw).expect_err(raw);
            assert!(
                err.contains("entry 2") || err.contains("#2"),
                "{raw}: {err}"
            );
            assert!(
                err.contains("string") && err.contains("object"),
                "message must say what is accepted: {err}"
            );
        }
    }

    #[test]
    fn object_without_a_url_or_with_a_blank_url_is_rejected() {
        for raw in [
            r#"[{"role": "cloud"}]"#,
            r#"[{"url": ""}]"#,
            r#"[{"url": "   "}]"#,
            r#"[{"url": 5}]"#,
        ] {
            let err = parse(raw).expect_err(raw);
            assert!(err.contains("url"), "{raw}: {err}");
        }
    }

    #[test]
    fn unknown_key_is_rejected_so_a_typo_never_falls_back_to_the_global_token() {
        let err = parse(r#"[{"url": "https://hub.example.com", "tokenfile": "/x"}]"#)
            .expect_err("typo'd key");
        assert!(
            err.contains("tokenfile"),
            "must name the offending key: {err}"
        );
    }

    #[test]
    fn both_token_sources_are_rejected_as_ambiguous() {
        let err = parse(r#"[{"url": "http://a:8400", "token_file": "/x", "token_env": "T"}]"#)
            .expect_err("ambiguous");
        assert!(
            err.contains("token_file") && err.contains("token_env"),
            "{err}"
        );
    }

    #[test]
    fn unknown_role_is_rejected_and_lists_the_valid_ones() {
        let err = parse(r#"[{"url": "http://a:8400", "role": "primary"}]"#).expect_err("role");
        assert!(err.contains("unknown role"), "{err}");
        assert!(
            !err.contains("primary"),
            "untrusted role is redacted: {err}"
        );
        assert!(
            err.contains("authoritative") && err.contains("fallback") && err.contains("cloud"),
            "{err}"
        );
    }

    #[test]
    fn blank_or_malformed_token_references_are_rejected() {
        for raw in [
            r#"[{"url": "http://a:8400", "token_file": ""}]"#,
            r#"[{"url": "http://a:8400", "token_file": "  "}]"#,
            r#"[{"url": "http://a:8400", "token_env": ""}]"#,
            r#"[{"url": "http://a:8400", "token_env": "HAS SPACE"}]"#,
            r#"[{"url": "http://a:8400", "token_env": "A=B"}]"#,
            r#"[{"url": "http://a:8400", "token_file": 5}]"#,
        ] {
            assert!(parse(raw).is_err(), "must reject {raw}");
        }
    }

    // ---------------------------------------------------------------
    // Role defaults and validation
    // ---------------------------------------------------------------

    fn urls(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    fn specs_by_url(specs: Vec<CandidateSpec>) -> BTreeMap<String, CandidateSpec> {
        specs.into_iter().map(|s| (s.url.clone(), s)).collect()
    }

    #[test]
    fn default_roles_are_authoritative_first_then_fallback() {
        let built = build_candidates(
            &urls(&["http://a:1", "http://b:1", "http://c:1"]),
            &BTreeMap::new(),
        );
        let roles: Vec<HubRole> = built.iter().map(|c| c.role).collect();
        assert_eq!(
            roles,
            vec![HubRole::Authoritative, HubRole::Fallback, HubRole::Fallback]
        );
        assert!(built.iter().all(|c| c.auth == CandidateAuth::Global));
    }

    #[test]
    fn explicit_role_and_token_source_override_the_defaults() {
        let specs = specs_by_url(
            parse(
                r#"["http://a:1", {"url": "https://cloud.example.com", "role": "cloud",
                    "token_env": "CLOUD_T"}]"#,
            )
            .expect("valid"),
        );
        let built = build_candidates(&urls(&["http://a:1", "https://cloud.example.com"]), &specs);
        assert_eq!(built[0].role, HubRole::Authoritative);
        assert_eq!(built[1].role, HubRole::Cloud);
        assert_eq!(built[1].auth, CandidateAuth::TokenEnv("CLOUD_T".to_owned()));
    }

    #[test]
    fn an_explicit_cloud_role_on_index_zero_is_never_authoritative() {
        let specs = specs_by_url(
            parse(r#"[{"url": "https://c.example.com", "role": "cloud", "token_env": "T"}, "http://b:1"]"#)
                .expect("valid"),
        );
        let built = build_candidates(&urls(&["https://c.example.com", "http://b:1"]), &specs);
        assert_eq!(built[0].role, HubRole::Cloud);
        assert_eq!(built[1].role, HubRole::Fallback);
        assert!(
            validate_candidates(&built).is_ok(),
            "no authoritative candidate at all is allowed (claims simply stay pending)"
        );
    }

    #[test]
    fn specs_for_urls_not_in_the_list_are_ignored() {
        let specs = specs_by_url(
            parse(r#"[{"url": "http://gone:1", "role": "cloud", "token_env": "T"}]"#)
                .expect("valid"),
        );
        let built = build_candidates(&urls(&["http://a:1"]), &specs);
        assert_eq!(built.len(), 1);
        assert_eq!(built[0].role, HubRole::Authoritative);
    }

    #[test]
    fn two_authoritative_candidates_are_rejected_naming_both() {
        let specs = specs_by_url(
            parse(r#"["http://a:1", {"url": "http://b:1", "role": "authoritative"}]"#)
                .expect("valid"),
        );
        let built = build_candidates(&urls(&["http://a:1", "http://b:1"]), &specs);
        let err = validate_candidates(&built).expect_err("two authorities");
        assert!(
            err.contains("http://a:1") && err.contains("http://b:1"),
            "{err}"
        );
    }

    #[test]
    fn a_single_authoritative_candidate_validates() {
        let built = build_candidates(&urls(&["http://a:1", "http://b:1"]), &BTreeMap::new());
        assert!(validate_candidates(&built).is_ok());
        assert!(validate_candidates(&[]).is_ok());
    }

    // ---------------------------------------------------------------
    // Public-host guard: positive and negative controls
    // ---------------------------------------------------------------

    #[test]
    fn https_public_hosts_are_flagged() {
        for url in [
            "https://agentbus.dtmventures.com",
            "https://agentbus.dtmventures.com/",
            "https://agentbus.dtmventures.com:8443/health",
            "https://example.com.",
            "https://EXAMPLE.COM",
            "HTTPS://agentbus.dtmventures.com",
            "https://user:pw@example.com/x",
            "https://10.0.0.1@evil.example.com/",
            "https://8.8.8.8",
            "https://172.15.255.255",
            "https://172.32.0.1",
            "https://11.0.0.1",
            "https://192.169.0.1",
            "https://100.63.255.255",
            "https://100.128.0.1",
            "https://[2606:4700::1111]",
            "https://134744072",
            "https://lan.example.com",
            "https://evil.lan.example.com",
            "https://x.internal.example.com",
            "https://home.arpa.example.com",
            "https://xlan.com",
            "https://",
        ] {
            assert!(
                is_public_https_target(url),
                "{url} must be treated as public"
            );
        }
    }

    #[test]
    fn https_private_hosts_are_not_flagged() {
        for url in [
            "https://localhost",
            "https://LOCALHOST:8443",
            "https://localhost.",
            "https://hub.localhost",
            "https://127.0.0.1",
            "https://127.1.2.3:8400",
            "https://10.0.0.1",
            "https://172.16.0.1",
            "https://172.31.255.255",
            "https://192.168.1.1",
            "https://169.254.10.20",
            "https://100.64.0.3",
            "https://100.127.255.255",
            "https://0.0.0.0",
            "https://[::1]",
            "https://[::1]:8400",
            "https://[fd00::1]",
            "https://[fc00::1]",
            "https://[fe80::1]",
            "https://[::ffff:10.0.0.1]",
            "https://asuspro13-p2p-dtm-p1gen7",
            "https://hub",
            "https://hub:8400/health",
            "https://agent-bus-hub.vigil.lan",
            "https://host.lan.",
            "https://HOST.LAN",
            "https://a.local",
            "https://x.internal",
            "https://x.home.arpa",
            "https://user:pw@10.0.0.1:8400/path",
        ] {
            assert!(
                !is_public_https_target(url),
                "{url} must be treated as private"
            );
        }
    }

    #[test]
    fn plain_http_is_never_flagged_by_the_https_guard() {
        for url in [
            "http://agentbus.dtmventures.com",
            "http://8.8.8.8:8400",
            "http://asuspro13-p2p-dtm-p1gen7:8400",
            "http://agent-bus-hub.vigil.lan:8400",
        ] {
            assert!(!is_public_https_target(url), "{url}");
        }
    }

    // ---------------------------------------------------------------
    // Token selection
    // ---------------------------------------------------------------

    #[test]
    fn string_entry_sends_the_global_token_exactly_as_before() {
        let env = FakeEnv::default();
        let token = string_candidate("http://asuspro13-p2p-dtm-p1gen7:8400")
            .authorization(Some(GLOBAL), &env)
            .expect("allowed")
            .expect("token present");
        assert_eq!(token.expose(), GLOBAL);
    }

    #[test]
    fn string_entry_without_a_global_token_sends_no_header() {
        let env = FakeEnv::default();
        let c = string_candidate("http://hub.lan:8400");
        assert_eq!(c.authorization(None, &env), Ok(None));
        assert_eq!(c.authorization(Some("   "), &env), Ok(None));
    }

    #[test]
    fn the_global_token_is_never_sent_to_a_public_https_host() {
        let env = FakeEnv::default();
        let c = string_candidate("https://agentbus.dtmventures.com");
        let err = c
            .authorization(Some(GLOBAL), &env)
            .expect_err("must be refused");
        assert!(
            err.0.contains("token_file"),
            "must tell the operator the fix: {err}"
        );
        assert!(!err.0.contains(GLOBAL), "must never echo a token: {err}");
        // Fail closed even with no global token configured: the candidate
        // has no credential of its own.
        assert!(c.authorization(None, &env).is_err());
    }

    #[test]
    fn the_global_token_is_never_sent_to_a_cloud_role_even_on_a_private_host() {
        let env = FakeEnv::default();
        let c = candidate(
            "http://localhost:8787",
            HubRole::Cloud,
            CandidateAuth::Global,
        );
        assert!(c.authorization(Some(GLOBAL), &env).is_err());
        assert!(c.authorization(None, &env).is_err());
    }

    #[test]
    fn private_https_host_still_receives_the_global_token() {
        // Negative control for the guard: it must not over-block.
        let env = FakeEnv::default();
        let c = string_candidate("https://agent-bus-hub.vigil.lan:8400");
        let token = c
            .authorization(Some(GLOBAL), &env)
            .expect("private https is allowed")
            .expect("token");
        assert_eq!(token.expose(), GLOBAL);
    }

    #[test]
    fn token_file_candidate_sends_only_the_file_token_and_trims_it() {
        let mut env = FakeEnv::default();
        env.files.insert(
            PathBuf::from("/secrets/cloud.token"),
            "  cloud-file-token \r\n".to_owned(),
        );
        let c = candidate(
            "https://agentbus.dtmventures.com",
            HubRole::Cloud,
            CandidateAuth::TokenFile("/secrets/cloud.token".to_owned()),
        );
        let token = c
            .authorization(Some(GLOBAL), &env)
            .expect("own token")
            .expect("present");
        assert_eq!(token.expose(), "cloud-file-token");
        assert_ne!(token.expose(), GLOBAL);
    }

    #[test]
    fn missing_token_file_skips_the_candidate_and_never_falls_back_to_global() {
        let env = FakeEnv::default();
        let c = candidate(
            "http://hub.lan:8400",
            HubRole::Fallback,
            CandidateAuth::TokenFile("/secrets/missing.token".to_owned()),
        );
        let err = c
            .authorization(Some(GLOBAL), &env)
            .expect_err("missing file must skip, not fall back");
        assert!(err.0.contains("/secrets/missing.token"), "{err}");
        assert!(!err.0.contains(GLOBAL), "{err}");
    }

    #[test]
    fn empty_or_malformed_token_file_is_rejected() {
        for content in [
            "",
            "   \n",
            "two words",
            "line1\nline2",
            "tab\there",
            "bell\u{7}",
        ] {
            let mut env = FakeEnv::default();
            env.files.insert(PathBuf::from("/t"), content.to_owned());
            let c = candidate(
                "http://hub.lan:8400",
                HubRole::Fallback,
                CandidateAuth::TokenFile("/t".to_owned()),
            );
            assert!(
                c.authorization(Some(GLOBAL), &env).is_err(),
                "{content:?} must be rejected"
            );
        }
    }

    #[test]
    fn token_file_tilde_is_expanded_against_the_home_directory() {
        for (configured, expected) in [
            ("~/.config/x.token", "/home/u/.config/x.token"),
            ("~\\x.token", "/home/u/x.token"),
            ("~", "/home/u"),
        ] {
            let mut env = FakeEnv {
                home: Some(PathBuf::from("/home/u")),
                ..FakeEnv::default()
            };
            env.files.insert(PathBuf::from(expected), "tok".to_owned());
            let c = candidate(
                "http://hub.lan:8400",
                HubRole::Fallback,
                CandidateAuth::TokenFile(configured.to_owned()),
            );
            let token = c
                .authorization(None, &env)
                .unwrap_or_else(|e| panic!("{configured}: {e}"))
                .expect("token");
            assert_eq!(token.expose(), "tok", "{configured}");
            assert_eq!(env.reads.borrow().as_slice(), [PathBuf::from(expected)]);
        }
    }

    #[test]
    fn only_a_leading_bare_tilde_is_expanded() {
        let mut env = FakeEnv {
            home: Some(PathBuf::from("/home/u")),
            ..FakeEnv::default()
        };
        env.files
            .insert(PathBuf::from("~other/x"), "tok".to_owned());
        env.files.insert(PathBuf::from("/a/~/x"), "tok".to_owned());
        for configured in ["~other/x", "/a/~/x"] {
            let c = candidate(
                "http://hub.lan:8400",
                HubRole::Fallback,
                CandidateAuth::TokenFile(configured.to_owned()),
            );
            assert!(c.authorization(None, &env).is_ok(), "{configured}");
        }
    }

    #[test]
    fn tilde_without_a_home_directory_skips_the_candidate() {
        let env = FakeEnv::default();
        let c = candidate(
            "http://hub.lan:8400",
            HubRole::Fallback,
            CandidateAuth::TokenFile("~/x.token".to_owned()),
        );
        let err = c.authorization(None, &env).expect_err("no home");
        assert!(err.0.contains('~'), "{err}");
    }

    #[test]
    fn token_env_candidate_reads_the_variable_and_trims_it() {
        let mut env = FakeEnv::default();
        env.vars
            .insert("HUB_T".to_owned(), "  env-token\n".to_owned());
        let c = candidate(
            "http://hub.lan:8400",
            HubRole::Fallback,
            CandidateAuth::TokenEnv("HUB_T".to_owned()),
        );
        let token = c
            .authorization(Some(GLOBAL), &env)
            .expect("own token")
            .expect("present");
        assert_eq!(token.expose(), "env-token");
    }

    #[test]
    fn unset_or_empty_token_env_skips_the_candidate_without_global_fallback() {
        let mut env = FakeEnv::default();
        env.vars.insert("EMPTY_T".to_owned(), "  ".to_owned());
        for name in ["UNSET_T", "EMPTY_T"] {
            let c = candidate(
                "http://hub.lan:8400",
                HubRole::Fallback,
                CandidateAuth::TokenEnv(name.to_owned()),
            );
            let err = c
                .authorization(Some(GLOBAL), &env)
                .expect_err("must skip, not fall back");
            assert!(err.0.contains(name), "{err}");
            assert!(!err.0.contains(GLOBAL), "{err}");
        }
    }

    #[test]
    fn debug_output_never_contains_a_token() {
        let token = BearerToken::new("super-sekret");
        assert!(!format!("{token:?}").contains("super-sekret"));
        let auth = HubAuth::new(
            Some("super-sekret".to_owned()),
            vec![string_candidate("http://hub.lan:8400")],
        );
        assert!(!format!("{auth:?}").contains("super-sekret"));
    }

    // ---------------------------------------------------------------
    // HubAuth: token per request URL
    // ---------------------------------------------------------------

    fn roaming_auth() -> (HubAuth, FakeEnv) {
        let mut env = FakeEnv::default();
        env.vars
            .insert("HUB_B_TOKEN".to_owned(), "b-token".to_owned());
        env.files
            .insert(PathBuf::from("/c.token"), "cloud-token".to_owned());
        let auth = HubAuth::new(
            Some(GLOBAL.to_owned()),
            vec![
                candidate(
                    "http://a.lan:8400",
                    HubRole::Authoritative,
                    CandidateAuth::Global,
                ),
                candidate(
                    "http://b.lan:8400/",
                    HubRole::Fallback,
                    CandidateAuth::TokenEnv("HUB_B_TOKEN".to_owned()),
                ),
                candidate(
                    "https://agentbus.dtmventures.com",
                    HubRole::Cloud,
                    CandidateAuth::TokenFile("/c.token".to_owned()),
                ),
            ],
        );
        (auth, env)
    }

    fn token_for(auth: &HubAuth, env: &FakeEnv, url: &str) -> Option<String> {
        auth.credential_for_url(url, env)
            .unwrap_or_else(|e| panic!("{url}: {e}"))
            .map(|t| t.expose().to_owned())
    }

    #[test]
    fn each_request_url_gets_only_its_own_candidates_token() {
        let (auth, env) = roaming_auth();
        assert_eq!(
            token_for(&auth, &env, "http://a.lan:8400/messages?x=1").as_deref(),
            Some(GLOBAL)
        );
        assert_eq!(
            token_for(&auth, &env, "http://b.lan:8400/health").as_deref(),
            Some("b-token")
        );
        assert_eq!(
            token_for(&auth, &env, "https://agentbus.dtmventures.com/sync/push").as_deref(),
            Some("cloud-token")
        );
    }

    #[test]
    fn a_base_url_only_matches_at_a_path_boundary() {
        let (auth, env) = roaming_auth();
        // A valid different port must not inherit the configured origin's
        // credential. Only recognized private service endpoints fall back.
        assert_eq!(
            token_for(&auth, &env, "http://a.lan:840/admin/service").as_deref(),
            Some(GLOBAL)
        );
        assert!(
            auth.credential_for_url("http://a.lan:84001/x", &env)
                .is_err(),
            "port 84001 is invalid"
        );
        assert!(
            auth.credential_for_url("http://a.lan:840/x", &env).is_err(),
            "unmatched non-service path is denied"
        );
        assert_eq!(
            token_for(&auth, &env, "http://b.lan:8400").as_deref(),
            Some("b-token"),
            "bare base URL must match despite the candidate's trailing slash"
        );
    }

    #[test]
    fn the_longest_matching_candidate_wins() {
        let mut env = FakeEnv::default();
        env.vars.insert("SUB".to_owned(), "sub-token".to_owned());
        let auth = HubAuth::new(
            Some(GLOBAL.to_owned()),
            vec![
                candidate(
                    "http://h.lan:8400",
                    HubRole::Authoritative,
                    CandidateAuth::Global,
                ),
                candidate(
                    "http://h.lan:8400/sub",
                    HubRole::Fallback,
                    CandidateAuth::TokenEnv("SUB".to_owned()),
                ),
            ],
        );
        assert_eq!(
            token_for(&auth, &env, "http://h.lan:8400/sub/health").as_deref(),
            Some("sub-token")
        );
        assert_eq!(
            token_for(&auth, &env, "http://h.lan:8400/other").as_deref(),
            Some(GLOBAL)
        );
    }

    #[test]
    fn unmatched_private_url_uses_the_global_token_like_service_admin_does_today() {
        let (auth, env) = roaming_auth();
        assert_eq!(
            token_for(&auth, &env, "http://localhost:8400/admin/service/control").as_deref(),
            Some(GLOBAL)
        );
    }

    #[test]
    fn unmatched_public_https_url_never_gets_the_global_token() {
        let (auth, env) = roaming_auth();
        let err = auth
            .credential_for_url("https://some-other-host.example.com/x", &env)
            .expect_err("guard applies to unmatched URLs too");
        assert!(!err.0.contains(GLOBAL));
    }

    #[test]
    fn a_skipped_candidate_url_stays_skipped_at_request_time() {
        let mut env = FakeEnv::default();
        env.vars.clear();
        let auth = HubAuth::new(
            Some(GLOBAL.to_owned()),
            vec![candidate(
                "http://b.lan:8400",
                HubRole::Fallback,
                CandidateAuth::TokenEnv("HUB_B_TOKEN".to_owned()),
            )],
        );
        assert!(
            auth.credential_for_url("http://b.lan:8400/x", &env)
                .is_err()
        );
    }
}
