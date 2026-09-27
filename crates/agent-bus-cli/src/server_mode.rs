//! Shared HTTP client helpers for CLI server-mode routing.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, Result};
use reqwest::StatusCode;

use agent_bus_core::hub::{HubBackend, ProbeInfo, resolve_hub};

use crate::settings::Settings;

#[cfg(feature = "server-mode")]
static SERVER_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[cfg(feature = "server-mode")]
const SERVER_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
#[cfg(feature = "server-mode")]
const SERVER_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Bearer token for cross-machine server-mode requests, resolved once from
/// [`Settings`] (env `AGENT_BUS_AUTH_TOKEN` → `config.json` → `None`).
#[cfg(feature = "server-mode")]
static SERVER_AUTH_TOKEN: OnceLock<Option<String>> = OnceLock::new();

/// Record the configured auth token so server-mode requests can attach
/// `Authorization: Bearer <token>`. Call once after settings are loaded and
/// before any server-mode HTTP call. No-op if the HTTP client was already
/// built.
#[cfg(feature = "server-mode")]
pub(crate) fn init_server_auth(settings: &Settings) {
    let _ = SERVER_AUTH_TOKEN.set(settings.auth_token.clone());
}

#[cfg(not(feature = "server-mode"))]
pub(crate) fn init_server_auth(_settings: &Settings) {}

#[cfg(feature = "server-mode")]
fn server_client() -> &'static reqwest::Client {
    SERVER_CLIENT.get_or_init(|| {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(SERVER_CONNECT_TIMEOUT)
            .timeout(SERVER_REQUEST_TIMEOUT);
        if let Some(Some(token)) = SERVER_AUTH_TOKEN.get()
            && let Ok(mut header) =
                reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
        {
            header.set_sensitive(true);
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, header);
            builder = builder.default_headers(headers);
        }
        builder.build().unwrap_or_else(|_| reqwest::Client::new())
    })
}

#[cfg(feature = "server-mode")]
fn server_auth_configured() -> bool {
    matches!(SERVER_AUTH_TOKEN.get(), Some(Some(token)) if !token.trim().is_empty())
}

#[cfg(feature = "server-mode")]
fn http_status_error(method: &str, url: &str, status: StatusCode, body: &str) -> anyhow::Error {
    let body = body.trim();
    let body = if body.is_empty() {
        "<empty response body>"
    } else {
        body
    };

    if status == StatusCode::UNAUTHORIZED && !server_auth_configured() {
        anyhow::anyhow!(
            "{method} {url} returned HTTP {status}: {body}. \
             The AgentHub service requires bearer-token auth, but this client has no \
             AGENT_BUS_AUTH_TOKEN/auth_token configured. Set AGENT_BUS_AUTH_TOKEN or \
             add auth_token to AGENT_BUS_CONFIG (~/.config/agent-bus/config.json)."
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
) -> Result<serde_json::Value> {
    let status = response.status();
    let text = response
        .text()
        .await
        .with_context(|| format!("{method} {url} response body read failed"))?;
    let parsed = serde_json::from_str::<serde_json::Value>(&text);

    if !status.is_success() {
        return match parsed {
            Ok(body) => Err(http_status_error(method, url, status, &body.to_string())),
            Err(_) => Err(http_status_error(method, url, status, &text)),
        };
    }

    parsed.with_context(|| {
        format!(
            "{method} {url} returned HTTP {status} but the response was not JSON: {}",
            text.trim()
        )
    })
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
    !settings.server_urls.is_empty()
}

#[cfg(not(feature = "server-mode"))]
pub(crate) fn use_server_mode(_settings: &Settings) -> bool {
    false
}

/// Probe one candidate hub's `/health` for [`resolve_hub`]. Returns `None` on
/// any failure — unreachable, timeout, non-2xx, or an unparseable body —
/// [`resolve_hub`] does not distinguish why a probe failed.
#[cfg(feature = "server-mode")]
fn probe_hub_health(url: &str) -> Option<ProbeInfo> {
    let health = http_get(&format!("{url}/health")).ok()?;
    Some(ProbeInfo {
        build_version: health
            .get("build_version")
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
    resolve_hub(&settings.server_urls, probe_hub_health)
}

#[cfg(not(feature = "server-mode"))]
pub(crate) fn active_hub_backend(_settings: &Settings) -> HubBackend {
    HubBackend::Local
}

/// Render an `HubBackend::Offline` state as the loud, explicit error used by
/// CLI commands that must refuse rather than silently read or write a local
/// store when no configured hub candidate answered.
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
    match active_hub_backend(settings) {
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
    let url = url.to_owned();
    run_server_future(async move {
        let response = server_client()
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url} failed"))?;
        decode_json_response("GET", &url, response).await
    })
}

#[cfg(not(feature = "server-mode"))]
pub(crate) fn http_get(_url: &str) -> Result<serde_json::Value> {
    anyhow::bail!("HTTP GET requires the 'server-mode' feature")
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
    let url = url.to_owned();
    let payload = body.clone();
    run_server_future(async move {
        let response = server_client()
            .post(&url)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("POST {url} failed"))?;
        decode_json_response("POST", &url, response).await
    })
}

#[cfg(not(feature = "server-mode"))]
pub(crate) fn http_post(_url: &str, _body: &serde_json::Value) -> Result<serde_json::Value> {
    anyhow::bail!("HTTP POST requires the 'server-mode' feature")
}

/// Performs a `PUT` request with a JSON body and returns the parsed JSON
/// response.
///
/// # Errors
///
/// Returns an error on network failure, non-2xx response, or JSON error.
#[cfg(feature = "server-mode")]
pub(crate) fn http_put(url: &str, body: &serde_json::Value) -> Result<serde_json::Value> {
    let url = url.to_owned();
    let payload = body.clone();
    run_server_future(async move {
        let response = server_client()
            .put(&url)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("PUT {url} failed"))?;
        decode_json_response("PUT", &url, response).await
    })
}

#[cfg(not(feature = "server-mode"))]
pub(crate) fn http_put(_url: &str, _body: &serde_json::Value) -> Result<serde_json::Value> {
    anyhow::bail!("HTTP PUT requires the 'server-mode' feature")
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
        run_server_future(async move {
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(timeout_seconds.max(1));
            let health_url = format!("{base_url}/health");

            loop {
                if let Ok(response) = server_client().get(&health_url).send().await
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
    /// Connect-level test, not a full TLS handshake against a real
    /// certificate: the discriminator is the shape of the failure against an
    /// address nothing listens on. Confirmed empirically before the `rustls`
    /// feature was added to this crate's `reqwest` dependency: connecting to
    /// `https://127.0.0.1:1/health` with NO TLS backend linked fails
    /// immediately with `"invalid URL, scheme is not http"` (reqwest refuses
    /// to even attempt the connection); with `rustls` linked, the same
    /// request instead fails with `"tcp connect error"` (connection
    /// refused), proving the scheme was accepted and a real socket connect
    /// was attempted. `anyhow::Error`'s alternate `{:#}` `Display` is used to
    /// print the full context chain (`http_get`'s `.with_context()` message
    /// plus the underlying `reqwest::Error` and its own source chain) --
    /// plain `{}`/`.to_string()` only prints the outermost context frame.
    #[test]
    fn server_client_accepts_https_candidates_for_the_cloud_hub_tier() {
        let result = http_get("https://127.0.0.1:1/health");
        let message = format!("{:#}", result.expect_err("port 1 must not have a listener"));

        assert!(
            !message.to_lowercase().contains("scheme is not http"),
            "https:// was rejected before any connection was attempted -- the \
             `rustls` feature is missing from agent-bus-cli's reqwest dependency; \
             got: {message}"
        );
        assert!(
            message.contains("tcp connect error"),
            "expected a real TCP connect attempt (and failure) once the scheme \
             was accepted; got: {message}"
        );
    }
}
