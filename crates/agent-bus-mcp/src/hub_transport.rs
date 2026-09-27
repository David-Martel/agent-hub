//! Blocking-from-async HTTP transport for [`agent_bus_core::remote_dispatch::RoutingDispatch`] (#78).
//!
//! `agent-bus-mcp`'s `call_tool`/`list_tools` handlers dispatch synchronously
//! (see `mcp.rs`), so this transport's trait methods are synchronous too. It
//! runs the actual `reqwest` request via [`tokio::task::block_in_place`] +
//! `Handle::block_on`, exactly like `agent-bus-cli`'s `server_mode.rs`
//! (`run_server_future`) already does — required because `agent-bus-mcp`
//! runs on a multi-thread Tokio runtime (`main.rs`), and calling a blocking
//! HTTP client directly from a runtime worker thread would panic
//! ("Cannot start a runtime from within a runtime").
//!
//! Credentials (the bearer token) are read once from [`Settings`] at
//! construction — never accepted as an inline MCP argument — matching the
//! issue's requirement that credentials load from env/config only.

use std::sync::OnceLock;
use std::time::Duration;

use agent_bus_core::error::{AgentBusError, Result};
use agent_bus_core::hub::ProbeInfo;
use agent_bus_core::remote_dispatch::RemoteMcpTransport;
use agent_bus_core::settings::Settings;
use serde_json::{Map, Value};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Reqwest-backed [`RemoteMcpTransport`] for the stdio MCP server.
pub(crate) struct HttpMcpTransport {
    client: reqwest::Client,
    auth_token: Option<String>,
}

impl HttpMcpTransport {
    pub(crate) fn new(settings: &Settings) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            auth_token: settings.auth_token.clone(),
        }
    }

    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth_token {
            Some(token) if !token.trim().is_empty() => builder.bearer_auth(token),
            _ => builder,
        }
    }
}

/// Run a future to completion from a synchronous context, whether or not one
/// is already on a Tokio runtime worker thread. Mirrors
/// `agent-bus-cli`'s `server_mode::run_server_future`.
fn run_future<T>(future: impl Future<Output = T>) -> T {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        tokio::task::block_in_place(|| handle.block_on(future))
    } else {
        // No ambient runtime (e.g. a unit test): build a throwaway one.
        static FALLBACK: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
        let runtime = FALLBACK.get_or_init(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("failed to build fallback runtime for hub transport")
        });
        runtime.block_on(future)
    }
}

impl RemoteMcpTransport for HttpMcpTransport {
    fn probe_health(&self, url: &str) -> Option<ProbeInfo> {
        run_future(async {
            let response = self
                .authed(self.client.get(format!("{url}/health")))
                .send()
                .await
                .ok()?;
            if !response.status().is_success() {
                return None;
            }
            let body: Value = response.json().await.ok()?;
            Some(ProbeInfo {
                build_version: body
                    .get("build_version")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
    }

    fn call_tool(&self, url: &str, name: &str, args: &Map<String, Value>) -> Result<Value> {
        run_future(async {
            let request_body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": name, "arguments": args},
            });
            let response = self
                .authed(self.client.post(format!("{url}/mcp")))
                .json(&request_body)
                .send()
                .await
                .map_err(|e| {
                    AgentBusError::Internal(format!(
                        "remote hub {url} unreachable for '{name}': {e}"
                    ))
                })?;

            let status = response.status();
            let body: Value = response.json().await.map_err(|e| {
                AgentBusError::Internal(format!(
                    "remote hub {url} returned a non-JSON response for '{name}' (HTTP {status}): {e}"
                ))
            })?;

            if let Some(error) = body.get("error") {
                return Err(AgentBusError::Internal(format!(
                    "remote hub {url} rejected '{name}': {error}"
                )));
            }
            if !status.is_success() {
                return Err(AgentBusError::Internal(format!(
                    "remote hub {url} returned HTTP {status} for '{name}': {body}"
                )));
            }

            // The hub's /mcp bridge wraps the tool's JSON result as a
            // stringified `content[0].text` block (see
            // `dispatch_mcp_method` in agent-bus-http's http.rs) to match
            // the MCP tools/call response shape. Unwrap it back to a value.
            let text = body
                .get("result")
                .and_then(|r| r.get("content"))
                .and_then(|c| c.get(0))
                .and_then(|block| block.get("text"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    AgentBusError::Internal(format!(
                        "remote hub {url} returned an unexpected tools/call shape for '{name}': {body}"
                    ))
                })?;

            serde_json::from_str(text).map_err(|e| {
                AgentBusError::Internal(format!(
                    "remote hub {url} returned unparseable tool result for '{name}': {e}"
                ))
            })
        })
    }
}
