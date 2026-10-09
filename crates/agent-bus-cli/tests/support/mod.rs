//! Explicit disposable targets for live integration tests.
//!
//! The provisioning harness owns fresh containers and the HTTP process. These
//! checks bind tests to that selected topology; they do not provision services.
#![allow(
    dead_code,
    reason = "Shared by synchronous and asynchronous test targets"
)]

use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use reqwest::{
    Url,
    header::{AUTHORIZATION, HeaderMap, HeaderValue},
};
use serde_json::Value;

pub struct Target {
    pub run_id: String,
    pub http_url: String,
    pub redis_url: String,
    pub database_url: String,
    pub token: String,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Target")
            .field("run_id", &self.run_id)
            .finish_non_exhaustive()
    }
}

impl Target {
    /// Validate all settings without opening files, sockets, or subprocesses.
    pub fn parse(mut get: impl FnMut(&str) -> Option<String>) -> Result<Self, String> {
        let mut required = |key: &str| {
            get(key)
                .filter(|v| !v.trim().is_empty())
                .ok_or_else(|| format!("explicit isolated integration target requires {key}"))
        };
        let run_id = required("AGENT_BUS_TEST_RUN_ID")?;
        if !(8..=48).contains(&run_id.len())
            || !run_id
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
        {
            return Err("invalid isolated run identity".into());
        }
        let http_url = required("AGENT_BUS_TEST_SERVER_URL")?;
        let redis_url = required("AGENT_BUS_TEST_REDIS_URL")?;
        let database_url = required("AGENT_BUS_TEST_DATABASE_URL")?;
        let token = required("AGENT_BUS_TEST_AUTH_TOKEN")?;
        let http = validate_url(&http_url, "http")?;
        let redis = validate_url(&redis_url, "redis")?;
        let database = validate_url(&database_url, "postgresql")?;
        if http.path() != "/"
            || !http.username().is_empty()
            || http.password().is_some()
            || redis.path() != "/0"
            || database.path() != format!("/agent_bus_test_{run_id}")
            || token.len() < 16
            || HeaderValue::from_str(&format!("Bearer {token}")).is_err()
        {
            return Err("invalid isolated endpoint path, database identity, or token".into());
        }
        let ports = [http.port(), redis.port(), database.port()];
        if ports[0] == ports[1] || ports[0] == ports[2] || ports[1] == ports[2] {
            return Err("isolated services must use distinct ports".into());
        }
        Ok(Self {
            run_id,
            http_url: http_url.trim_end_matches('/').to_owned(),
            redis_url,
            database_url,
            token,
        })
    }

    pub fn validate_health(&self, body: &Value) -> Result<(), String> {
        if body["ok"] != true || body["database_ok"] != true || body["storage_ready"] != true {
            return Err("isolated HTTP, Redis and PostgreSQL must all be healthy".into());
        }
        if body["maintenance"]["service_agent_id"] != format!("agent-bus-test-{}", self.run_id) {
            return Err("HTTP service does not belong to the selected isolated run".into());
        }
        for (field, expected) in [
            ("redis_url", &self.redis_url),
            ("database_url", &self.database_url),
        ] {
            let actual = body[field]
                .as_str()
                .ok_or("HTTP health lacks backend identity")?;
            let actual = Url::parse(actual)
                .map_err(|error| format!("HTTP health has invalid backend identity: {error}"))?;
            let expected = Url::parse(expected)
                .map_err(|error| format!("invalid selected backend: {error}"))?;
            if (
                actual.scheme(),
                actual.host_str(),
                actual.port(),
                actual.path(),
            ) != (
                expected.scheme(),
                expected.host_str(),
                expected.port(),
                expected.path(),
            ) {
                return Err("HTTP backend differs from selected isolated backend".into());
            }
        }
        Ok(())
    }
}

fn validate_url(value: &str, scheme: &str) -> Result<Url, String> {
    const FORBIDDEN_PORTS: &[u16] = &[6379, 6380, 5432, 5300, 8400, 8401, 18400];
    let url =
        Url::parse(value).map_err(|error| format!("invalid isolated endpoint URL: {error}"))?;
    if url.scheme() != scheme
        || url.host_str() != Some("localhost")
        || url
            .port()
            .is_none_or(|p| p < 1024 || FORBIDDEN_PORTS.contains(&p))
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(
            "isolated endpoint must use localhost and an explicit non-production port".into(),
        );
    }
    Ok(url)
}

pub fn target() -> &'static Target {
    static TARGET: OnceLock<Target> = OnceLock::new();
    TARGET.get_or_init(|| {
        Target::parse(|key| std::env::var(key).ok())
            .expect("refusing non-isolated integration target")
    })
}

pub fn base_url() -> &'static str {
    &target().http_url
}

pub fn is_agent_setting(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy()
        .to_ascii_uppercase()
        .starts_with("AGENT_BUS_")
}

pub fn auth_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", target().token)).expect("validated test token"),
    );
    headers
}

pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .default_headers(auth_headers())
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .expect("HTTP test client")
}

pub fn blocking_http_client() -> reqwest::blocking::Client {
    reqwest::blocking::Client::builder()
        .default_headers(auth_headers())
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .expect("blocking HTTP test client")
}

pub async fn require_http(client: &reqwest::Client) {
    let body = client
        .get(format!("{}/health", base_url()))
        .send()
        .await
        .expect("isolated HTTP unavailable")
        .error_for_status()
        .expect("isolated health rejected")
        .json::<Value>()
        .await
        .expect("isolated health JSON");
    target()
        .validate_health(&body)
        .expect("isolated service identity and backing stores");
    assert_eq!(
        client
            .get(format!("{}/admin/service", base_url()))
            .send()
            .await
            .expect("isolated admin unavailable")
            .status(),
        reqwest::StatusCode::OK,
        "isolated admin authentication and control are required"
    );
}

pub fn require_http_blocking(client: &reqwest::blocking::Client) {
    let body = client
        .get(format!("{}/health", base_url()))
        .send()
        .expect("isolated HTTP unavailable")
        .error_for_status()
        .expect("isolated health rejected")
        .json::<Value>()
        .expect("isolated health JSON");
    target()
        .validate_health(&body)
        .expect("isolated service identity and backing stores");
    assert_eq!(
        client
            .get(format!("{}/admin/service", base_url()))
            .send()
            .expect("isolated admin unavailable")
            .status(),
        reqwest::StatusCode::OK,
        "isolated admin authentication and control are required"
    );
}

pub fn agent_bus_binary() -> Command {
    static CONFIG: OnceLock<tempfile::TempPath> = OnceLock::new();
    let selected = target();
    require_http_blocking(&blocking_http_client());
    let config = CONFIG.get_or_init(|| {
        let file = tempfile::NamedTempFile::new().expect("isolated CLI config");
        std::fs::write(file.path(), b"{}").expect("write isolated CLI config");
        file.into_temp_path()
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_agent-bus"));
    for (key, _) in std::env::vars_os() {
        if is_agent_setting(&key) {
            command.env_remove(key);
        }
    }
    for key in [
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "ALL_PROXY",
        "http_proxy",
        "https_proxy",
        "all_proxy",
    ] {
        command.env_remove(key);
    }
    command
        .env("AGENT_BUS_CONFIG", config.as_os_str())
        .env("AGENT_BUS_REDIS_URL", &selected.redis_url)
        .env("AGENT_BUS_DATABASE_URL", &selected.database_url)
        .env("AGENT_BUS_AUTH_TOKEN", &selected.token)
        .env("AGENT_BUS_STARTUP_ENABLED", "false");
    command
}
