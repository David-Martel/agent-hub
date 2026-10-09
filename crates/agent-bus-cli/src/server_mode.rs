//! Shared HTTP client helpers for CLI server-mode routing.

#[cfg(feature = "server-mode")]
use std::collections::HashMap;
#[cfg(feature = "server-mode")]
use std::sync::{Arc, Mutex, OnceLock};
#[cfg(any(feature = "server-mode", windows))]
use std::time::Duration;

#[cfg(any(feature = "server-mode", windows))]
use anyhow::Context as _;
use anyhow::Result;
#[cfg(feature = "server-mode")]
use reqwest::StatusCode;

#[cfg(feature = "server-mode")]
use agent_bus_core::hub::HubBackend;
#[cfg(feature = "server-mode")]
use agent_bus_core::hub::{
    ProbeInfo, invalidate_configured_cache, resolve_authoritative_hub, resolve_configured_hub,
};
#[cfg(feature = "server-mode")]
use agent_bus_core::hub_candidates::{HubAuth, SystemEnv};
#[cfg(feature = "server-mode")]
use agent_bus_core::settings::redact_url;

use crate::settings::Settings;

#[cfg(feature = "server-mode")]
type ClientResult = std::result::Result<Arc<reqwest::Client>, String>;
#[cfg(feature = "server-mode")]
static SERVER_CLIENTS: OnceLock<Mutex<HashMap<(u64, bool), ClientResult>>> = OnceLock::new();

#[cfg(feature = "server-mode")]
const SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Startup configuration; credentials themselves are resolved for each request.
#[cfg(feature = "server-mode")]
static SERVER_SETTINGS: OnceLock<Settings> = OnceLock::new();

/// Record candidate configuration before the first server-mode request.
#[cfg(feature = "server-mode")]
pub(crate) fn init_server_auth(settings: &Settings) {
    let _ = SERVER_SETTINGS.set(settings.clone());
}

#[cfg(not(feature = "server-mode"))]
pub(crate) fn init_server_auth(_settings: &Settings) {}

#[cfg(feature = "server-mode")]
fn server_settings() -> Settings {
    SERVER_SETTINGS
        .get()
        .cloned()
        .unwrap_or_else(Settings::from_env)
}

#[cfg(feature = "server-mode")]
fn client_for_settings(settings: &Settings) -> Result<Arc<reqwest::Client>> {
    client_with_retry_policy(settings, true)
}

#[cfg(feature = "server-mode")]
fn client_with_retry_policy(
    settings: &Settings,
    allow_protocol_retries: bool,
) -> Result<Arc<reqwest::Client>> {
    let mut clients = SERVER_CLIENTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_error| anyhow::anyhow!("guarded HTTP client cache lock poisoned"))?;
    clients
        .entry((settings.probe_connect_timeout_ms, allow_protocol_retries))
        .or_insert_with(|| {
            let builder = reqwest::Client::builder()
                .connect_timeout(Duration::from_millis(settings.probe_connect_timeout_ms))
                .timeout(SERVER_REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none());
            // A consuming Task DELETE must disable even reqwest's default
            // protocol-NACK retries. Other existing calls keep their policy.
            let builder = if allow_protocol_retries {
                builder
            } else {
                builder.retry(reqwest::retry::never())
            };
            builder.build().map(Arc::new).map_err(|error| {
                format!("failed to build guarded server-mode HTTP client: {error}")
            })
        })
        .clone()
        .map_err(|error| anyhow::anyhow!("{error}"))
}

#[cfg(feature = "server-mode")]
fn server_client() -> Result<Arc<reqwest::Client>> {
    client_for_settings(&server_settings())
}

#[cfg(feature = "server-mode")]
fn authed_request(
    settings: &Settings,
    url: &str,
    request: reqwest::RequestBuilder,
) -> Result<reqwest::RequestBuilder> {
    reject_hub_config_error(settings)?;
    let auth = HubAuth::new(
        settings.auth_token.clone(),
        settings.effective_hub_candidates(),
    );
    let token = auth
        .credential_for_url(url, &SystemEnv)
        .map_err(|reason| anyhow::anyhow!("refusing request to {}: {reason}", redact_url(url)))?;
    Ok(match token {
        Some(token) => request.bearer_auth(token.expose()),
        None => request,
    })
}

#[cfg(feature = "server-mode")]
fn reject_hub_config_error(settings: &Settings) -> Result<()> {
    if let Some(error) = &settings.hub_config_error {
        anyhow::bail!("invalid hub candidate configuration: {error}");
    }
    if !(1..=30_000).contains(&settings.probe_connect_timeout_ms) {
        anyhow::bail!("probe_connect_timeout_ms must be between 1 and 30000");
    }
    Ok(())
}

#[cfg(feature = "server-mode")]
async fn send_json(
    settings: &Settings,
    client: &reqwest::Client,
    method: reqwest::Method,
    url: &str,
    payload: Option<&serde_json::Value>,
    timeout: Option<Duration>,
) -> Result<serde_json::Value> {
    let mut request = authed_request(settings, url, client.request(method.clone(), url))?;
    if let Some(payload) = payload {
        request = request.json(payload);
    }
    if let Some(timeout) = timeout {
        request = request.timeout(timeout);
    }
    let request = request
        .build()
        .context("failed to build guarded hub request")?;
    let sent_token = request
        .headers()
        .get(reqwest::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok())
        .and_then(|header| header.strip_prefix("Bearer "))
        .map(str::to_owned);
    let response = client
        .execute(request)
        .await
        .inspect_err(|_| invalidate_configured_cache(settings))
        .with_context(|| format!("{method} {url} failed"))?;
    decode_json_response(method.as_str(), url, response, sent_token.as_deref())
        .await
        .inspect_err(|_| invalidate_configured_cache(settings))
}

#[cfg(feature = "server-mode")]
fn http_status_error(method: &str, url: &str, status: StatusCode, body: &str) -> anyhow::Error {
    let url = redact_url(url);
    let body = body.trim();
    let body = if body.is_empty() {
        "<empty response body>"
    } else {
        body
    };

    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        anyhow::anyhow!(
            "{method} {url} returned HTTP {status}. \
             The AgentHub service requires bearer-token auth. Check this candidate's \
             token_file/token_env or the permitted AGENT_BUS_AUTH_TOKEN/auth_token configuration; \
             never reuse the on-site token for a public cloud candidate."
        )
    } else {
        anyhow::anyhow!("{method} {url} returned HTTP {status}: {body}")
    }
}

#[cfg(feature = "server-mode")]
async fn decode_json_response(
    method: &str,
    url: &str,
    response: reqwest::Response,
    sent_token: Option<&str>,
) -> Result<serde_json::Value> {
    let status = response.status();
    let text = response
        .text()
        .await
        .with_context(|| format!("{method} {url} response body read failed"))?;
    let parsed = serde_json::from_str::<serde_json::Value>(&text);

    if !status.is_success() {
        let diagnostic = match &parsed {
            Ok(body) => redact_json_body(body, sent_token).to_string(),
            Err(_) => "<non-JSON response body omitted>".to_owned(),
        };
        return Err(http_status_error(method, url, status, &diagnostic));
    }

    parsed.map_err(|error| {
        anyhow::anyhow!("{method} {} returned HTTP {status} but the response was not JSON (line {}, column {}); response body omitted",
            redact_url(url), error.line(), error.column())
    })
}

#[cfg(feature = "server-mode")]
fn redact_sent_token(text: &str, sent_token: Option<&str>) -> String {
    match sent_token.filter(|token| !token.is_empty()) {
        Some(token) => text.replace(token, "<redacted>"),
        None => text.to_owned(),
    }
}

#[cfg(feature = "server-mode")]
fn redact_json_body(body: &serde_json::Value, sent_token: Option<&str>) -> serde_json::Value {
    use serde_json::Value;
    match body {
        Value::String(text) => Value::String(redact_sent_token(text, sent_token)),
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| redact_json_body(value, sent_token))
                .collect(),
        ),
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, value)| {
                    (
                        redact_sent_token(key, sent_token),
                        redact_json_body(value, sent_token),
                    )
                })
                .collect(),
        ),
        _ => body.clone(),
    }
}

#[cfg(feature = "server-mode")]
fn run_server_future<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(future))
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build server-mode runtime")?
            .block_on(future)
    }
}

/// Returns `true` when the caller should route through the HTTP service
/// instead of connecting to Redis directly.
///
/// Checks the whole ordered candidate list (`server_urls`), not just the
/// single-URL `server_url` alias, so `AGENT_BUS_SERVER_URLS` alone (with no
/// `AGENT_BUS_SERVER_URL`) is enough to opt a command into server mode (#78).
#[cfg(feature = "server-mode")]
pub(crate) fn use_server_mode(settings: &Settings) -> bool {
    settings.hub_config_error.is_some() || !settings.server_urls.is_empty()
}

/// Probe one candidate hub's `/health` for [`resolve_configured_hub`]. Returns `None` on
/// any failure — unreachable, timeout, non-2xx, or an unparseable body —
/// [`resolve_configured_hub`] does not distinguish why a probe failed.
#[cfg(feature = "server-mode")]
fn probe_hub_health(settings: &Settings, url: &str) -> Option<ProbeInfo> {
    let client = client_for_settings(settings).ok()?;
    let health_url = format!("{url}/health");
    let health = match run_server_future(send_json(
        settings,
        &client,
        reqwest::Method::GET,
        &health_url,
        None,
        Some(Duration::from_millis(settings.probe_connect_timeout_ms)),
    )) {
        Ok(health) => health,
        Err(error) => {
            // send_json already scrubs the actual sent credential from
            // diagnostics; never log request headers or raw response bodies.
            tracing::debug!(hub = %redact_url(url), error = %format!("{error:#}"), "hub health probe failed");
            return None;
        }
    };
    Some(ProbeInfo {
        build_version: health
            .get("build_version")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        hub_identity: health
            .get("hub_identity")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

/// Resolve which hub backend `settings.server_urls` currently points at
/// (#78): the first reachable candidate (`Remote`, `authoritative` iff it was
/// index 0), `Offline` if candidates are configured but none answered, or
/// `Local` if none are configured at all. Never falls back to a local Redis
/// read/write when candidates were configured but unreachable — that is
/// exactly the split-brain island #78 reports.
#[cfg(feature = "server-mode")]
pub(crate) fn active_hub_backend(settings: &Settings) -> HubBackend {
    resolve_configured_hub(settings, &mut |url: &str| probe_hub_health(settings, url))
}

/// Render an `HubBackend::Offline` state as the loud, explicit error used by
/// CLI commands that must refuse rather than silently read or write a local
/// store when no configured hub candidate answered.
#[cfg(feature = "server-mode")]
pub(crate) fn offline_error(command: &str, tried: &[String]) -> anyhow::Error {
    anyhow::anyhow!(
        "{command}: offline: no authoritative hub reachable (tried {tried:?}). This client is \
         configured with remote hub candidates (AGENT_BUS_SERVER_URLS/AGENT_BUS_SERVER_URL) and \
         has no local bus of its own; refusing to silently read or write a local store and \
         report it as fleet state."
    )
}

/// Render the "claim pending" error used specifically for claim-authority
/// operations (`claim`, `renew-claim`, `release-claim`, `resolve`) -- the
/// operator's exact wording, distinct from `offline_error` because a caller
/// polling for a claim needs to know this is a retryable "not yet", not a
/// hard failure. Used both when no candidate answered at all AND when a
/// candidate answered but was not the authoritative (first-priority) one:
/// once a second, later hub tier exists (e.g. a Cloudflare-hosted fallback),
/// there must be exactly one claims authority, never a grant against
/// whichever candidate happened to answer.
#[cfg(feature = "server-mode")]
pub(crate) fn claim_pending_error(command: &str, tried: &[String]) -> anyhow::Error {
    anyhow::anyhow!(
        "{command}: claim pending: no authoritative hub reachable (tried {tried:?}); exclusive \
         claims cannot be granted, renewed, released or resolved against a non-authoritative \
         fallback or while offline. Retry once the first-priority hub in \
         AGENT_BUS_SERVER_URLS/AGENT_BUS_SERVER_URL is reachable."
    )
}

/// Resolve the base URL for an ordinary (non-claim-authority) HTTP request:
/// `send`/`read`/`presence`/`presence-list`/`batch-send`/`knock`/
/// `compact-context`. Any reachable candidate is fine here -- a fallback
/// answering is still the fleet, just not the highest-priority path to it.
/// Returns the loud offline error when no candidate answered.
///
/// # Errors
/// Returns [`offline_error`] when every candidate is unreachable, or an
/// internal error if called while in local-only mode (a caller bug: every
/// call site guards on [`use_server_mode`] first).
#[cfg(feature = "server-mode")]
pub(crate) fn resolve_hub_url(settings: &Settings, command: &str) -> Result<String> {
    reject_hub_config_error(settings)?;
    match active_hub_backend(settings) {
        HubBackend::Remote { url, .. } => Ok(url),
        HubBackend::Offline { tried } => Err(offline_error(command, &tried)),
        HubBackend::Local => Err(anyhow::anyhow!(
            "{command}: resolve_hub_url called with no hub candidates configured (local-only \
             mode) -- this is a caller bug, not an offline condition; check use_server_mode first"
        )),
    }
}

/// Resolve the base URL for a claim-AUTHORITY operation: `claim`,
/// `renew-claim`, `release-claim`, `resolve`. Only the authoritative
/// (first-priority) candidate may grant, renew, release or resolve an
/// exclusive claim -- never a reachable-but-lower-priority fallback, and
/// never a silent local grant. Both "no candidate reachable" and "a
/// candidate answered but is not authoritative" yield the same "claim
/// pending" error: a caller polling for the claim should retry either way,
/// not distinguish the two.
///
/// # Errors
/// Returns [`claim_pending_error`] unless the authoritative candidate itself
/// answered, or an internal error if called while in local-only mode (a
/// caller bug: every call site guards on [`use_server_mode`] first).
#[cfg(feature = "server-mode")]
pub(crate) fn resolve_authoritative_claim_url(
    settings: &Settings,
    command: &str,
) -> Result<String> {
    reject_hub_config_error(settings)?;
    match resolve_authoritative_hub(settings, &mut |url: &str| probe_hub_health(settings, url)) {
        HubBackend::Remote {
            url,
            authoritative: true,
            ..
        } => Ok(url),
        HubBackend::Remote { tried, .. } | HubBackend::Offline { tried } => {
            Err(claim_pending_error(command, &tried))
        }
        HubBackend::Local => Err(anyhow::anyhow!(
            "{command}: resolve_authoritative_claim_url called with no hub candidates configured \
             (local-only mode) -- this is a caller bug, not an offline condition; check \
             use_server_mode first"
        )),
    }
}

/// Performs a `GET` request and returns the parsed JSON body.
///
/// # Errors
///
/// Returns an error if the URL is unreachable, the response is not 2xx,
/// or JSON deserialisation fails.
#[cfg(feature = "server-mode")]
pub(crate) fn http_get(url: &str) -> Result<serde_json::Value> {
    let settings = server_settings();
    let client = server_client()?;
    run_server_future(send_json(
        &settings,
        &client,
        reqwest::Method::GET,
        url,
        None,
        None,
    ))
}

/// Performs a `POST` request with a JSON body and returns the parsed JSON
/// response.
///
/// # Errors
///
/// Returns an error if the request fails, the server returns a non-2xx status,
/// or JSON deserialisation fails.
#[cfg(feature = "server-mode")]
pub(crate) fn http_post(url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    let settings = server_settings();
    let client = server_client()?;
    run_server_future(send_json(
        &settings,
        &client,
        reqwest::Method::POST,
        url,
        Some(body),
        None,
    ))
}

/// Performs a `PUT` request with a JSON body and returns the parsed JSON
/// response.
///
/// # Errors
///
/// Returns an error on network failure, non-2xx response, or JSON error.
#[cfg(feature = "server-mode")]
pub(crate) fn http_put(url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    let settings = server_settings();
    let client = server_client()?;
    run_server_future(send_json(
        &settings,
        &client,
        reqwest::Method::PUT,
        url,
        Some(body),
        None,
    ))
}

/// Send one consuming DELETE and decode its response without retrying.
///
/// # Errors
/// Returns an error on network failure, non-2xx response, or JSON error.
#[cfg(feature = "server-mode")]
pub(crate) fn http_delete(url: &str) -> Result<serde_json::Value> {
    let settings = server_settings();
    let client = client_with_retry_policy(&settings, false)?;
    run_server_future(send_json(
        &settings,
        &client,
        reqwest::Method::DELETE,
        url,
        None,
        None,
    ))
}

pub(crate) fn resolved_service_base_url(settings: &Settings, base_url: Option<&str>) -> String {
    #[cfg(feature = "server-mode")]
    {
        base_url
            .filter(|value| !value.trim().is_empty())
            .map(str::to_owned)
            .or_else(|| settings.server_url.clone())
            .unwrap_or_else(|| format!("http://{}:8400", settings.server_host))
    }

    #[cfg(not(feature = "server-mode"))]
    {
        let _ = (settings, base_url);
        "http://localhost:8400".to_owned()
    }
}

pub(crate) fn post_service_action(
    base_url: &str,
    action: &str,
    reason: Option<&str>,
) -> Result<serde_json::Value> {
    #[cfg(feature = "server-mode")]
    {
        let url = format!("{base_url}/admin/service/control");
        let mut payload = serde_json::json!({ "action": action });
        if let Some(reason) = reason.filter(|value| !value.trim().is_empty()) {
            payload["reason"] = serde_json::Value::String(reason.to_owned());
        }
        http_post(&url, &payload)
    }

    #[cfg(not(feature = "server-mode"))]
    {
        let _ = (base_url, action, reason);
        anyhow::bail!("service admin HTTP actions require the 'server-mode' feature")
    }
}

pub(crate) fn wait_for_health(base_url: &str, timeout_seconds: u64) -> Result<serde_json::Value> {
    #[cfg(feature = "server-mode")]
    {
        let base_url = base_url.to_owned();
        let settings = server_settings();
        reject_hub_config_error(&settings)?;
        run_server_future(async move {
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(timeout_seconds.max(1));
            let health_url = format!("{base_url}/health");

            loop {
                let request =
                    authed_request(&settings, &health_url, server_client()?.get(&health_url))?;
                if let Ok(response) = request.send().await
                    && response.status().is_success()
                {
                    let body: serde_json::Value = response
                        .json()
                        .await
                        .context("HTTP health response JSON decode failed")?;
                    if body.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
                        return Ok(body);
                    }
                }

                if tokio::time::Instant::now() >= deadline {
                    anyhow::bail!("timed out waiting for healthy service at {health_url}");
                }

                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        })
    }

    #[cfg(not(feature = "server-mode"))]
    {
        let _ = (base_url, timeout_seconds);
        anyhow::bail!("health polling requires the 'server-mode' feature")
    }
}

#[cfg(windows)]
pub(crate) fn query_windows_service_state(service_name: &str) -> Result<Option<String>> {
    let output = std::process::Command::new("sc.exe")
        .args(["query", service_name])
        .output()
        .with_context(|| format!("failed to query Windows service '{service_name}'"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        let combined = format!("{stdout}\n{stderr}");
        if combined.contains("1060") || combined.contains("does not exist") {
            return Ok(None);
        }
        anyhow::bail!("sc.exe query {service_name} failed: {}", combined.trim());
    }

    for line in stdout.lines() {
        if let Some((_, rest)) = line.split_once(':')
            && line.contains("STATE")
        {
            let state = rest
                .split_whitespace()
                .nth(1)
                .map_or_else(|| rest.trim().to_owned(), str::to_owned);
            return Ok(Some(state));
        }
    }

    Ok(Some("UNKNOWN".to_owned()))
}

#[cfg(not(windows))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "signature must match the #[cfg(windows)] variant that can fail"
)]
pub(crate) fn query_windows_service_state(_service_name: &str) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(windows)]
pub(crate) fn wait_for_windows_service_state(
    service_name: &str,
    desired_state: &str,
    timeout_seconds: u64,
) -> Result<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_seconds.max(1));
    let desired = desired_state.to_uppercase();

    loop {
        match query_windows_service_state(service_name)? {
            Some(state) if state.eq_ignore_ascii_case(&desired) => return Ok(state),
            Some(_) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(500));
            }
            Some(state) => anyhow::bail!(
                "timed out waiting for Windows service '{service_name}' to reach {desired}; last state={state}"
            ),
            None => anyhow::bail!("Windows service '{service_name}' is not installed"),
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn wait_for_windows_service_state(
    _service_name: &str,
    _desired_state: &str,
    _timeout_seconds: u64,
) -> Result<String> {
    anyhow::bail!("Windows service control is only supported on Windows")
}

#[cfg(windows)]
pub(crate) fn sc_action(service_name: &str, action: &str) -> Result<()> {
    let output = std::process::Command::new("sc.exe")
        .args([action, service_name])
        .output()
        .with_context(|| format!("failed to run sc.exe {action} {service_name}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!(
        "sc.exe {action} {service_name} failed: {}",
        format!("{stdout}\n{stderr}").trim()
    )
}

#[cfg(not(windows))]
pub(crate) fn sc_action(_service_name: &str, _action: &str) -> Result<()> {
    anyhow::bail!("Windows service control is only supported on Windows")
}

pub(crate) fn service_status_payload(
    base_url: &str,
    configured_service_name: &str,
    windows_service_state: Option<&str>,
    admin_status: Option<&serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "base_url": base_url,
        "service_name": configured_service_name,
        "windows_service_state": windows_service_state,
        "admin": admin_status,
    })
}

#[cfg(all(test, feature = "server-mode"))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn isolated_settings() -> Settings {
        let mut settings = Settings::from_env();
        settings.server_url = None;
        settings.server_urls.clear();
        settings.hub_candidates.clear();
        settings.hub_config_error = None;
        settings.hub_cache_ttl_seconds = 0;
        settings.auth_token = None;
        settings.probe_connect_timeout_ms = 750;
        settings
    }

    fn capture_mock(
        status: u16,
        extra_headers: String,
    ) -> (String, std::thread::JoinHandle<String>) {
        let body = if status == 401 {
            r#"{"error":"reflected synthetic-cli-rejected-token"}"#
        } else {
            r#"{"ok":true}"#
        };
        capture_mock_body(status, extra_headers, body)
    }

    fn capture_mock_body(
        status: u16,
        extra_headers: String,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind CLI fixture");
        listener.set_nonblocking(true).expect("nonblocking fixture");
        let address = listener.local_addr().expect("fixture address");
        let handle = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "CLI fixture accept timed out"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("CLI fixture accept failed: {error}"),
                }
            };
            stream
                .set_nonblocking(false)
                .expect("blocking fixture stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("fixture read timeout");
            let mut reader = BufReader::new(stream.try_clone().expect("clone fixture stream"));
            let mut headers = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).expect("read request header");
                if line.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    length = value.trim().parse::<usize>().expect("content length");
                }
                headers.push_str(&line);
            }
            reader
                .read_exact(&mut vec![0; length])
                .expect("drain request body");
            write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}", body.len()).expect("write fixture response");
            stream.flush().expect("flush fixture response");
            headers.to_lowercase()
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn real_cli_post_and_put_resolve_rotated_candidate_file_without_global_token() {
        use agent_bus_core::hub_candidates::{CandidateAuth, HubCandidate, HubRole};
        let (first, first_handle) = capture_mock(200, String::new());
        let (second, second_handle) = capture_mock(200, String::new());
        let file = tempfile::NamedTempFile::new().expect("synthetic token file");
        std::fs::write(file.path(), "cli-candidate-before").expect("write fixture token");
        let mut settings = isolated_settings();
        settings.auth_token = Some("global-onsite-must-not-leak".to_owned());
        settings.server_urls = vec![first.clone(), second.clone()];
        settings.hub_candidates = settings
            .server_urls
            .iter()
            .map(|url| HubCandidate {
                url: url.clone(),
                role: HubRole::Cloud,
                auth: CandidateAuth::TokenFile(file.path().to_string_lossy().into_owned()),
                sites: Vec::new(),
                hub: None,
            })
            .collect();
        let client = client_for_settings(&settings).expect("guarded client");
        let body = serde_json::json!({"fixture":true});
        run_server_future(send_json(
            &settings,
            &client,
            reqwest::Method::POST,
            &format!("{first}/messages"),
            Some(&body),
            None,
        ))
        .expect("first request");
        std::fs::write(file.path(), "cli-candidate-after").expect("rotate fixture token");
        run_server_future(send_json(
            &settings,
            &client,
            reqwest::Method::PUT,
            &format!("{second}/presence/fixture"),
            Some(&body),
            None,
        ))
        .expect("second request");
        let before = first_handle.join().expect("first fixture completion");
        let after = second_handle.join().expect("second fixture completion");
        assert!(before.starts_with("post /messages "));
        assert!(after.starts_with("put /presence/fixture "));
        assert!(before.contains("authorization: bearer cli-candidate-before\r\n"));
        assert!(after.contains("authorization: bearer cli-candidate-after\r\n"));
        assert!(!before.contains("global-onsite-must-not-leak"));
        assert!(!after.contains("global-onsite-must-not-leak"));
    }

    #[test]
    fn real_cli_get_does_not_follow_redirect_or_forward_onsite_token() {
        let target = TcpListener::bind("127.0.0.1:0").expect("bind redirect target");
        target.set_nonblocking(true).expect("nonblocking target");
        let (source, handle) = capture_mock(
            302,
            format!(
                "Location: http://{}/messages\r\n",
                target.local_addr().expect("target address")
            ),
        );
        let mut settings = isolated_settings();
        settings.auth_token = Some("synthetic-cli-onsite-token".to_owned());
        settings.server_urls = vec![source.clone()];
        let client = client_for_settings(&settings).expect("guarded client");
        let error = run_server_future(send_json(
            &settings,
            &client,
            reqwest::Method::GET,
            &format!("{source}/messages"),
            None,
            None,
        ))
        .expect_err("redirect must fail");
        assert!(error.to_string().contains("HTTP 302"));
        let headers = handle.join().expect("redirect fixture completion");
        assert!(headers.contains("authorization: bearer synthetic-cli-onsite-token\r\n"));
        assert_eq!(
            target
                .accept()
                .expect_err("redirect target must not connect")
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn real_cli_request_rejects_invalid_configuration_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind invalid-config fixture");
        listener.set_nonblocking(true).expect("nonblocking fixture");
        let url = format!(
            "http://{}/health",
            listener.local_addr().expect("fixture address")
        );
        let mut settings = isolated_settings();
        settings.hub_config_error = Some("unknown hub option".to_owned());
        let client = client_for_settings(&settings).expect("guarded client");
        let error = run_server_future(send_json(
            &settings,
            &client,
            reqwest::Method::GET,
            &url,
            None,
            None,
        ))
        .expect_err("invalid configuration must fail");
        assert!(error.to_string().contains("unknown hub option"));
        assert!(
            use_server_mode(&settings),
            "invalid candidates must never choose local mode"
        );
        assert_eq!(
            listener
                .accept()
                .expect_err("invalid config must not connect")
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn real_cli_unauthorized_response_keeps_candidate_auth_guidance_without_token() {
        let (url, handle) = capture_mock(401, "WWW-Authenticate: Bearer\r\n".to_owned());
        let mut settings = isolated_settings();
        settings.auth_token = Some("synthetic-cli-rejected-token".to_owned());
        settings.server_urls = vec![url.clone()];
        let client = client_for_settings(&settings).expect("guarded client");
        let error = run_server_future(send_json(
            &settings,
            &client,
            reqwest::Method::GET,
            &format!("{url}/messages"),
            None,
            None,
        ))
        .expect_err("401 must fail");
        assert!(error.to_string().contains("token_file/token_env"));
        assert!(error.to_string().contains("HTTP 401"));
        assert!(!error.to_string().contains("synthetic-cli-rejected-token"));
        let headers = handle.join().expect("401 fixture completion");
        assert!(headers.contains("authorization: bearer synthetic-cli-rejected-token\r\n"));
    }

    #[test]
    fn cli_client_factory_reuses_the_exact_client_for_the_same_timeout() {
        let settings = isolated_settings();
        let first = client_for_settings(&settings).expect("first guarded client");
        let second = client_for_settings(&settings).expect("second guarded client");
        assert!(
            Arc::ptr_eq(&first, &second),
            "probes must reuse the actual TLS client and connection pool"
        );
        let mut changed = settings.clone();
        changed.probe_connect_timeout_ms += 1;
        let different = client_for_settings(&changed).expect("different timeout client");
        assert!(
            !Arc::ptr_eq(&first, &different),
            "different timeout policies require distinct clients"
        );
    }

    #[test]
    fn reflected_bearer_in_escaped_json_and_malformed_success_is_never_in_cli_errors() {
        for (status, body) in [
            (
                500,
                r#"{"error":"maintenance \u0061bc\/def", "\u0061bc\/def":"nested reflection \u0061bc\/def"}"#,
            ),
            (200, "not-json abc/def"),
        ] {
            let (url, handle) = capture_mock_body(status, String::new(), body);
            let mut settings = isolated_settings();
            settings.auth_token = Some("abc/def".to_owned());
            settings.server_urls = vec![url.clone()];
            let client = client_for_settings(&settings).expect("guarded client");
            let error = run_server_future(send_json(
                &settings,
                &client,
                reqwest::Method::GET,
                &format!("{url}/messages"),
                None,
                None,
            ))
            .expect_err("fixture response must fail");
            let message = format!("{error:#}");
            assert!(!message.contains("abc/def"), "sent token must be redacted");
            if status == 500 {
                assert!(
                    message.contains("maintenance"),
                    "useful diagnostic must remain"
                );
                assert!(
                    message.contains("<redacted>"),
                    "decoded keys and strings must be scrubbed"
                );
            } else {
                assert!(message.contains("response body omitted"));
            }
            let headers = handle.join().expect("reflection fixture completion");
            assert!(
                headers.contains("authorization: bearer abc/def\r\n"),
                "fixture must receive the token actually tested"
            );
        }
    }

    #[test]
    fn successful_cli_business_payload_is_not_changed_by_diagnostic_redaction() {
        let (url, handle) = capture_mock_body(200, String::new(), r#"{"business":"abc/def"}"#);
        let mut settings = isolated_settings();
        settings.auth_token = Some("abc/def".to_owned());
        settings.server_urls = vec![url.clone()];
        let client = client_for_settings(&settings).expect("guarded client");
        let value = run_server_future(send_json(
            &settings,
            &client,
            reqwest::Method::GET,
            &format!("{url}/messages"),
            None,
            None,
        ))
        .expect("valid business response");
        assert_eq!(
            value["business"], "abc/def",
            "only diagnostic copies may be scrubbed"
        );
        assert!(
            handle
                .join()
                .expect("fixture completion")
                .contains("authorization: bearer abc/def\r\n")
        );
    }

    #[test]
    fn http_status_error_explains_missing_bearer_token() {
        let message = http_status_error(
            "POST",
            "http://localhost:8400/messages",
            StatusCode::UNAUTHORIZED,
            "unauthorized: missing or invalid bearer token",
        )
        .to_string();

        assert!(message.contains("HTTP 401 Unauthorized"));
        assert!(message.contains("AGENT_BUS_AUTH_TOKEN"));
        assert!(message.contains("auth_token"));
    }

    #[test]
    fn http_status_error_preserves_non_auth_status_body() {
        let message = http_status_error(
            "GET",
            "http://localhost:8400/messages",
            StatusCode::SERVICE_UNAVAILABLE,
            "maintenance",
        )
        .to_string();

        assert!(message.contains("HTTP 503 Service Unavailable"));
        assert!(message.contains("maintenance"));
        assert!(!message.contains("requires bearer-token auth"));
    }

    /// Proves an `https://` candidate (e.g. a Cloudflare-hosted
    /// `https://agentbus.dtmventures.com` tier, item 5) is handled by an
    /// HTTPS-capable connector through the production `server_client()` used
    /// by every `server-mode` HTTP call (`http_get`/`http_post`/`http_put`),
    /// not a hand-rolled client.
    ///
    /// The real TCP peer records a TLS `ClientHello` before closing without a
    /// certificate. This proves TLS support independently of platform-specific
    /// connection-refused/timeout diagnostics, without trusting a test CA.
    #[test]
    fn server_client_accepts_https_candidates_for_the_cloud_hub_tier() {
        let mut settings = isolated_settings();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind TLS fixture");
        listener
            .set_nonblocking(true)
            .expect("nonblocking TLS fixture");
        let url = format!("https://{}", listener.local_addr().expect("TLS address"));
        let peer = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "TLS fixture accept timed out"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("TLS fixture accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).expect("blocking TLS stream");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("TLS read deadline");
            let mut header = [0; 6];
            stream
                .read_exact(&mut header)
                .expect("read TLS ClientHello");
            header
        });
        settings.server_urls = vec![url.clone()];
        let client = client_for_settings(&settings).expect("guarded HTTPS client");
        let result = run_server_future(send_json(
            &settings,
            &client,
            reqwest::Method::GET,
            &format!("{url}/health"),
            None,
            None,
        ));
        let message = format!(
            "{:#}",
            result.expect_err("peer closes without a TLS certificate")
        );
        let header = peer.join().expect("TLS peer completion");
        assert_eq!(header[0], 0x16, "TLS handshake record required");
        assert_eq!(header[1], 0x03, "TLS record major version required");
        assert_eq!(header[5], 0x01, "TLS ClientHello required");
        assert!(u16::from_be_bytes([header[3], header[4]]) >= 4);

        assert!(
            !message.to_lowercase().contains("scheme is not http"),
            "https:// was rejected before any connection was attempted -- the \
             `rustls` feature is missing from agent-bus-cli's reqwest dependency; \
             got: {message}"
        );
    }
}
