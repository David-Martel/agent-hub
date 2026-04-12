//! Shared HTTP client helpers for CLI server-mode routing.

use std::sync::OnceLock;
use std::time::Duration;

use anyhow::{Context as _, Result};

use crate::settings::Settings;

#[cfg(feature = "server-mode")]
static SERVER_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

#[cfg(feature = "server-mode")]
fn server_client() -> &'static reqwest::Client {
    SERVER_CLIENT.get_or_init(reqwest::Client::new)
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
#[cfg(feature = "server-mode")]
pub(crate) fn use_server_mode(settings: &Settings) -> bool {
    settings.server_url.is_some()
}

#[cfg(not(feature = "server-mode"))]
pub(crate) fn use_server_mode(_settings: &Settings) -> bool {
    false
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
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .context("HTTP response JSON decode failed")?;
        if !status.is_success() {
            anyhow::bail!("HTTP {status}: {body}");
        }
        Ok(body)
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
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .context("HTTP response JSON decode failed")?;
        if !status.is_success() {
            anyhow::bail!("HTTP {status}: {body}");
        }
        Ok(body)
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
        let status = response.status();
        let body: serde_json::Value = response
            .json()
            .await
            .context("HTTP response JSON decode failed")?;
        if !status.is_success() {
            anyhow::bail!("HTTP {status}: {body}");
        }
        Ok(body)
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
