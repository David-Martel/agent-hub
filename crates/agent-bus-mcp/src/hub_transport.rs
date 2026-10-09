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
//! Candidate credentials are resolved for each request from startup configuration,
//! never accepted as inline MCP arguments or attached as client default headers.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use agent_bus_core::error::{AgentBusError, Result};
use agent_bus_core::hub::ProbeInfo;
use agent_bus_core::hub::invalidate_configured_cache;
use agent_bus_core::hub_candidates::{HubAuth, SystemEnv};
use agent_bus_core::remote_dispatch::RemoteMcpTransport;
use agent_bus_core::settings::Settings;
use agent_bus_core::settings::redact_url;
use serde_json::{Map, Value};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Process-wide `reqwest::Client`, built once and cheaply cloned by every
/// [`HttpMcpTransport`]. Mirrors `agent-bus-cli`'s `server_mode::SERVER_CLIENT`
/// pattern (#78 review item 6): `mcp.rs`'s `call_tool_now` constructs a fresh
/// `HttpMcpTransport` on every MCP tool call, and `reqwest::Client` is
/// `Clone` + internally `Arc`'d (connection pool, TLS config), so sharing one
/// avoids rebuilding the TLS stack and a fresh connection pool per call.
///
/// Deliberately does NOT carry bearer tokens: [`HttpMcpTransport`] keeps
/// candidate configuration per-instance and attaches credentials per-request via
/// [`HttpMcpTransport::authed`], because tests in this module construct
/// transports with different tokens in the same process — baking the token
/// into the shared client would make the first-constructed token "stick"
/// for every later instance.
type ClientResult = std::result::Result<reqwest::Client, String>;
static SHARED_CLIENTS: OnceLock<Mutex<HashMap<u64, ClientResult>>> = OnceLock::new();

fn shared_client(connect_timeout_ms: u64) -> ClientResult {
    let mut clients = SHARED_CLIENTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_error| "guarded HTTP client cache lock poisoned".to_owned())?;
    clients
        .entry(connect_timeout_ms)
        .or_insert_with(|| {
            reqwest::Client::builder()
                .connect_timeout(Duration::from_millis(connect_timeout_ms))
                .timeout(REQUEST_TIMEOUT)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| format!("failed to build guarded hub HTTP client: {error}"))
        })
        .clone()
}

/// Reqwest-backed [`RemoteMcpTransport`] for the stdio MCP server.
pub(crate) struct HttpMcpTransport {
    client: ClientResult,
    settings: Settings,
}

impl HttpMcpTransport {
    pub(crate) fn new(settings: &Settings) -> Self {
        Self {
            client: shared_client(settings.probe_connect_timeout_ms),
            settings: settings.clone(),
        }
    }

    fn client(&self) -> Result<&reqwest::Client> {
        self.client
            .as_ref()
            .map_err(|error| AgentBusError::Internal(error.clone()))
    }

    fn authed(
        &self,
        url: &str,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder> {
        if let Some(error) = &self.settings.hub_config_error {
            return Err(AgentBusError::InvalidParams(format!(
                "invalid hub candidate configuration: {error}"
            )));
        }
        if !(1..=30_000).contains(&self.settings.probe_connect_timeout_ms) {
            return Err(AgentBusError::InvalidParams(
                "probe_connect_timeout_ms must be between 1 and 30000".to_owned(),
            ));
        }
        let auth = HubAuth::new(
            self.settings.auth_token.clone(),
            self.settings.effective_hub_candidates(),
        );
        let token = auth.credential_for_url(url, &SystemEnv).map_err(|reason| {
            AgentBusError::InvalidParams(format!(
                "refusing request to {}: {reason}",
                redact_url(url)
            ))
        })?;
        Ok(match token {
            Some(token) => builder.bearer_auth(token.expose()),
            None => builder,
        })
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
            let health_url = format!("{url}/health");
            let response = self
                .authed(&health_url, self.client().ok()?.get(&health_url))
                .ok()?
                .timeout(Duration::from_millis(
                    self.settings.probe_connect_timeout_ms,
                ))
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
                hub_identity: body
                    .get("hub_identity")
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
            let client = self.client()?;
            let request = self
                .authed(&format!("{url}/mcp"), client.post(format!("{url}/mcp")))?
                .json(&request_body)
                .build()
                .map_err(|error| {
                    AgentBusError::Internal(format!("failed to build guarded MCP request: {error}"))
                })?;
            let sent_token = request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|header| header.to_str().ok())
                .and_then(|header| header.strip_prefix("Bearer "))
                .map(str::to_owned);
            let url = redact_url(url);
            let response = client
                .execute(request)
                .await
                .inspect_err(|_| invalidate_configured_cache(&self.settings))
                .map_err(|e| {
                    AgentBusError::Internal(format!(
                        "remote hub {url} unreachable for '{name}': {}",
                        redact_sent_token(&e.to_string(), sent_token.as_deref())
                    ))
                })?;

            decode_tool_response(response, &url, name, sent_token.as_deref())
                .await
                .inspect_err(|_| invalidate_configured_cache(&self.settings))
        })
    }
}

async fn decode_tool_response(
    response: reqwest::Response,
    url: &str,
    name: &str,
    sent_token: Option<&str>,
) -> Result<Value> {
    let status = response.status();
    if matches!(
        status,
        reqwest::StatusCode::UNAUTHORIZED | reqwest::StatusCode::FORBIDDEN
    ) {
        return Err(AgentBusError::Internal(format!(
            "remote hub {url} returned HTTP {status} for '{name}': candidate credential rejected; check this candidate's token_file/token_env configuration"
        )));
    }
    if status == reqwest::StatusCode::NOT_IMPLEMENTED {
        return Err(AgentBusError::Internal(format!(
            "remote hub {url} returned HTTP 501 for '{name}': HTTP MCP is not implemented by this hub tier"
        )));
    }
    let body: Value = response.json().await.map_err(|e| {
        AgentBusError::Internal(format!(
            "remote hub {url} returned a non-JSON response for '{name}' (HTTP {status}): {}",
            redact_sent_token(&e.to_string(), sent_token)
        ))
    })?;

    if let Some(error) = body.get("error") {
        return Err(AgentBusError::Internal(format!(
            "remote hub {url} rejected '{name}': {}",
            redact_json_body(error, sent_token)
        )));
    }
    if !status.is_success() {
        return Err(AgentBusError::Internal(format!(
            "remote hub {url} returned HTTP {status} for '{name}': {}",
            redact_json_body(&body, sent_token)
        )));
    }

    // The hub's /mcp bridge wraps the tool's JSON result as a
    // stringified `content[0].text` block (see
    // `dispatch_mcp_method` in agent-bus-http's http.rs) to match
    // the MCP tools/call response shape. Unwrap it back to a value.
    unwrap_tool_result(&body, url, name, sent_token)
}

fn unwrap_tool_result(
    body: &Value,
    url: &str,
    name: &str,
    sent_token: Option<&str>,
) -> Result<Value> {
    let text = body
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(|c| c.get(0))
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AgentBusError::Internal(format!(
                "remote hub {url} returned an unexpected tools/call shape for '{name}': {}",
                redact_json_body(body, sent_token)
            ))
        })?;

    serde_json::from_str(text).map_err(|e| {
                AgentBusError::Internal(format!(
                    "remote hub {url} returned unparseable tool result for '{name}' (line {}, column {}); response body omitted", e.line(), e.column()
                ))
            })
}

fn redact_sent_token(text: &str, sent_token: Option<&str>) -> String {
    match sent_token.filter(|token| !token.is_empty()) {
        Some(token) => text.replace(token, "<redacted>"),
        None => text.to_owned(),
    }
}

fn redact_json_body(body: &Value, sent_token: Option<&str>) -> Value {
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

#[cfg(test)]
mod tests {
    //! Exercises the REAL reqwest client and REAL JSON-RPC request/response
    //! shape against a minimal in-process mock hub -- unlike
    //! `agent_bus_core::remote_dispatch`'s tests (which use a `FakeTransport`
    //! and never touch the network at all), this is what actually proves
    //! `HttpMcpTransport` sends a `tools/call` POST /mcp body the hub
    //! understands and correctly unwraps `result.content[0].text` back into
    //! the tool's JSON value -- the exact path issue #78's acceptance
    //! criterion ("prove `bus_health` returns the ASUS build in a fresh
    //! process") depends on.
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn accept_bounded(listener: &TcpListener) -> std::net::TcpStream {
        listener
            .set_nonblocking(true)
            .expect("nonblocking fixture listener");
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .expect("blocking fixture stream");
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .expect("fixture read timeout");
                    stream
                        .set_write_timeout(Some(Duration::from_secs(2)))
                        .expect("fixture write timeout");
                    return stream;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "fixture accept timed out"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        }
    }

    /// Capture real requests with bounded accept/read timeouts so auth failures
    /// cannot leave a fixture thread waiting forever.
    fn capture_mock(
        status: u16,
        extra_headers: String,
        count: usize,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let body = if status == 401 {
            r#"{"error":"reflected synthetic-rejected-token"}"#
        } else {
            r#"{"build_version":"fixture","jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{}"}]}}"#
        };
        capture_mock_body(status, extra_headers, count, body)
    }

    fn capture_mock_body(
        status: u16,
        extra_headers: String,
        count: usize,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind capture fixture");
        listener.set_nonblocking(true).expect("nonblocking fixture");
        let address = listener.local_addr().expect("capture address");
        let handle = std::thread::spawn(move || {
            let mut captured = Vec::new();
            for _ in 0..count {
                let deadline = std::time::Instant::now() + Duration::from_secs(3);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "capture accept timed out"
                            );
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("capture accept failed: {error}"),
                    }
                };
                stream
                    .set_nonblocking(false)
                    .expect("blocking capture stream");
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .expect("read timeout");
                let mut reader = BufReader::new(stream.try_clone().expect("clone fixture stream"));
                let mut headers = String::new();
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read request headers");
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        content_length = value.trim().parse::<usize>().expect("content length");
                    }
                    headers.push_str(&line);
                }
                reader
                    .read_exact(&mut vec![0; content_length])
                    .expect("read request body");
                write!(stream, "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}", body.len()).expect("write fixture response");
                stream.flush().expect("flush fixture response");
                captured.push(headers.to_lowercase());
            }
            captured
        });
        (format!("http://{address}"), handle)
    }

    fn isolated_settings() -> Settings {
        let mut settings = Settings::from_env();
        settings.server_url = None;
        settings.server_urls.clear();
        settings.hub_candidates.clear();
        settings.hub_config_error = None;
        settings.auth_token = None;
        settings.hub_cache_ttl_seconds = 0;
        // Generous on purpose: these tests exercise loopback mocks, not timeouts.
        // 750 ms is below a Windows SYN retransmit (~1 s), so one dropped
        // loopback SYN on a loaded CI host failed with "error sending request".
        settings.probe_connect_timeout_ms = 10_000;
        settings
    }

    struct TokenFixture(std::path::PathBuf);

    impl TokenFixture {
        fn new(token: &str) -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "agent-bus-transport-token-{}-{unique}",
                std::process::id()
            ));
            std::fs::write(&path, token).expect("write synthetic fixture token");
            Self(path)
        }
    }

    impl Drop for TokenFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn candidate_token_file_is_resolved_again_for_each_real_request() {
        use agent_bus_core::hub_candidates::{CandidateAuth, HubCandidate, HubRole};
        let (url, handle) = capture_mock(200, String::new(), 2);
        let fixture = TokenFixture::new("candidate-token-before");
        let mut settings = isolated_settings();
        settings.auth_token = Some("onsite-token-must-not-leak".to_owned());
        settings.server_urls = vec![url.clone()];
        settings.hub_candidates = vec![HubCandidate {
            url: url.clone(),
            role: HubRole::Cloud,
            auth: CandidateAuth::TokenFile(fixture.0.to_string_lossy().into_owned()),
            sites: Vec::new(),
            hub: None,
        }];
        let transport = HttpMcpTransport::new(&settings);
        transport
            .call_tool(&url, "bus_health", &Map::new())
            .expect("first candidate request");
        std::fs::write(&fixture.0, "candidate-token-after").expect("rotate synthetic token");
        transport
            .call_tool(&url, "bus_health", &Map::new())
            .expect("second candidate request");
        let requests = handle.join().expect("capture fixture completion");
        assert!(requests[0].contains("authorization: bearer candidate-token-before\r\n"));
        assert!(requests[1].contains("authorization: bearer candidate-token-after\r\n"));
        assert!(
            requests
                .iter()
                .all(|headers| !headers.contains("onsite-token-must-not-leak"))
        );
    }

    #[test]
    fn redirect_is_rejected_without_forwarding_credentials_or_connecting_to_target() {
        let target = TcpListener::bind("127.0.0.1:0").expect("bind redirect target");
        target
            .set_nonblocking(true)
            .expect("nonblocking redirect target");
        let (url, handle) = capture_mock(
            302,
            format!(
                "Location: http://{}/mcp\r\n",
                target.local_addr().expect("target address")
            ),
            1,
        );
        let mut settings = isolated_settings();
        settings.auth_token = Some("synthetic-onsite-token".to_owned());
        settings.server_urls = vec![url.clone()];
        let transport = HttpMcpTransport::new(&settings);
        let error = transport
            .call_tool(&url, "bus_health", &Map::new())
            .expect_err("redirect must fail");
        assert!(error.to_string().contains("HTTP 302"), "{error}");
        let requests = handle.join().expect("redirect fixture completion");
        assert!(requests[0].contains("authorization: bearer synthetic-onsite-token\r\n"));
        assert_eq!(
            target
                .accept()
                .expect_err("redirect target must not be contacted")
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn invalid_candidate_configuration_refuses_probe_and_tool_before_network_io() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind invalid-config target");
        listener
            .set_nonblocking(true)
            .expect("nonblocking invalid-config target");
        let url = format!("http://{}", listener.local_addr().expect("target address"));
        let mut settings = isolated_settings();
        settings.hub_config_error = Some("unknown candidate field".to_owned());
        let transport = HttpMcpTransport::new(&settings);
        assert!(transport.probe_health(&url).is_none());
        let error = transport
            .call_tool(&url, "bus_health", &Map::new())
            .expect_err("invalid config must fail");
        assert!(error.to_string().contains("unknown candidate field"));
        assert_eq!(
            listener
                .accept()
                .expect_err("invalid config must not connect")
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn bearer_rejection_is_not_reported_as_a_successful_tool_result() {
        let (url, handle) = capture_mock(401, "WWW-Authenticate: Bearer\r\n".to_owned(), 1);
        let mut settings = isolated_settings();
        settings.auth_token = Some("synthetic-rejected-token".to_owned());
        settings.server_urls = vec![url.clone()];
        let transport = HttpMcpTransport::new(&settings);
        let error = transport
            .call_tool(&url, "bus_health", &Map::new())
            .expect_err("401 must fail");
        assert!(error.to_string().contains("HTTP 401"));
        assert!(!error.to_string().contains("synthetic-rejected-token"));
        let requests = handle.join().expect("401 fixture completion");
        assert!(requests[0].contains("authorization: bearer synthetic-rejected-token\r\n"));
    }

    #[test]
    fn escaped_bearer_reflection_is_scrubbed_from_rpc_http_and_shape_errors() {
        for (status, body, expected) in [
            (
                200,
                r#"{"jsonrpc":"2.0","error":{"message":"rejected \u0061bc\/def", "\u0061bc\/def":"reflection"}}"#,
                "rejected",
            ),
            (
                500,
                r#"{"message":"maintenance \u0061bc\/def", "\u0061bc\/def":"reflection"}"#,
                "HTTP 500",
            ),
            (
                200,
                r#"{"message":"unexpected \u0061bc\/def", "\u0061bc\/def":"reflection"}"#,
                "unexpected tools/call shape",
            ),
        ] {
            let (url, handle) = capture_mock_body(status, String::new(), 1, body);
            let transport = transport_with_token(Some("abc/def"), &url);
            let error = transport
                .call_tool(&url, "bus_health", &Map::new())
                .expect_err("fixture response must fail");
            let message = error.to_string();
            assert!(
                !message.contains("abc/def"),
                "sent token must not be included in error"
            );
            assert!(
                message.contains("<redacted>"),
                "decoded JSON keys and strings must be scrubbed"
            );
            assert!(message.contains(expected), "useful diagnostic must remain");
            let requests = handle.join().expect("reflection fixture completion");
            assert!(
                requests[0].contains("authorization: bearer abc/def\r\n"),
                "fixture must receive the token actually tested"
            );
        }
    }

    #[test]
    fn malformed_rpc_and_tool_result_errors_do_not_reflect_the_sent_token() {
        for body in [
            "not-json abc/def",
            r#"{"jsonrpc":"2.0","result":{"content":[{"text":"not-json abc/def"}]}}"#,
        ] {
            let (url, handle) = capture_mock_body(200, String::new(), 1, body);
            let transport = transport_with_token(Some("abc/def"), &url);
            let error = transport
                .call_tool(&url, "bus_health", &Map::new())
                .expect_err("malformed payload must fail");
            assert!(!error.to_string().contains("abc/def"));
            assert!(
                error.to_string().contains("JSON")
                    || error.to_string().contains("unparseable tool result")
            );
            assert!(
                handle.join().expect("fixture completion")[0]
                    .contains("authorization: bearer abc/def\r\n")
            );
        }
    }

    #[test]
    fn successful_mcp_business_payload_is_not_changed_by_diagnostic_redaction() {
        let body =
            r#"{"jsonrpc":"2.0","result":{"content":[{"text":"{\"business\":\"abc/def\"}"}]}}"#;
        let (url, handle) = capture_mock_body(200, String::new(), 1, body);
        let transport = transport_with_token(Some("abc/def"), &url);
        let value = transport
            .call_tool(&url, "bus_health", &Map::new())
            .expect("valid business result");
        assert_eq!(
            value["business"], "abc/def",
            "only diagnostic copies may be scrubbed"
        );
        assert!(
            handle.join().expect("fixture completion")[0]
                .contains("authorization: bearer abc/def\r\n")
        );
    }

    #[test]
    fn deferred_cloud_mcp_is_reported_as_unsupported() {
        let (url, handle) = capture_mock(501, String::new(), 1);
        let mut settings = isolated_settings();
        settings.server_urls = vec![url.clone()];
        let transport = HttpMcpTransport::new(&settings);
        let error = transport
            .call_tool(&url, "bus_health", &Map::new())
            .expect_err("501 must fail");
        assert!(error.to_string().contains("HTTP MCP is not implemented"));
        handle.join().expect("501 fixture completion");
    }

    #[test]
    fn cached_client_builder_failure_is_returned_without_an_insecure_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind builder-failure target");
        listener
            .set_nonblocking(true)
            .expect("nonblocking builder-failure target");
        let url = format!("http://{}", listener.local_addr().expect("target address"));
        let transport = HttpMcpTransport {
            client: Err("fixture client construction failed".to_owned()),
            settings: isolated_settings(),
        };
        assert!(transport.probe_health(&url).is_none());
        let error = transport
            .call_tool(&url, "bus_health", &Map::new())
            .expect_err("builder failure must be returned");
        assert!(
            error
                .to_string()
                .contains("fixture client construction failed")
        );
        assert_eq!(
            listener
                .accept()
                .expect_err("builder failure must not connect")
                .kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    /// `reqwest::Error`'s `Display` only prints its own top-level message
    /// ("error sending request for url ..."); the discriminating text lives
    /// deeper in the `source()` chain (hyper-util's connector, then the OS
    /// error). Concatenate the whole chain so a substring assertion can see
    /// it, mirroring what `anyhow::Error`'s `{:#}` alternate `Display` does
    /// for `anyhow` errors (this is a bare `reqwest::Error`, not `anyhow`).
    fn full_error_chain(err: &(dyn std::error::Error + 'static)) -> String {
        let mut message = err.to_string();
        let mut cause = err.source();
        while let Some(source) = cause {
            message.push_str(": ");
            message.push_str(&source.to_string());
            cause = source.source();
        }
        message
    }

    /// Proves the real transport emits a TLS `ClientHello` for HTTPS.
    ///
    /// The fixture closes after reading the handshake prefix; certificate
    /// validation and a completed TLS session are outside this test's scope.
    #[test]
    fn https_candidate_reaches_a_real_connect_attempt_not_a_scheme_rejection() {
        let transport = transport_with_token(None, "http://localhost:8400");
        let rt = tokio::runtime::Runtime::new().expect("build test runtime");
        let (result, hello) = rt.block_on(async {
            use tokio::io::AsyncReadExt;

            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind HTTPS fixture");
            let addr = listener.local_addr().expect("fixture local address");
            let receive_hello = async {
                let (mut stream, _) = listener.accept().await?;
                let mut hello = [0_u8; 6];
                stream.read_exact(&mut hello).await?;
                Ok::<_, std::io::Error>(hello)
            };

            tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(
                    transport
                        .client()
                        .expect("guarded test client")
                        .get(format!("https://{addr}/health"))
                        .send(),
                    receive_hello
                )
            })
            .await
            .expect("HTTPS fixture and request must finish within five seconds")
        });

        let hello = hello.expect("fixture must receive a TLS handshake prefix");
        assert_eq!(hello[0], 0x16, "expected TLS handshake record");
        assert_eq!(hello[1], 0x03, "expected TLS record version major");
        assert!(hello[2] <= 0x03, "expected TLS legacy record version");
        assert!(u16::from_be_bytes([hello[3], hello[4]]) > 0);
        assert_eq!(hello[5], 0x01, "expected ClientHello handshake message");

        let err = result.expect_err("fixture closes before completing TLS");
        let message = full_error_chain(&err);
        assert!(
            !err.is_builder(),
            "HTTPS must reach the connector rather than fail request building: {message}"
        );
    }

    #[test]
    fn unsupported_scheme_is_rejected_before_connecting() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind scheme fixture");
        listener
            .set_nonblocking(true)
            .expect("make scheme fixture nonblocking");
        let addr = listener.local_addr().expect("fixture local address");
        let transport = transport_with_token(None, "http://localhost:8400");
        let rt = tokio::runtime::Runtime::new().expect("build test runtime");
        let err = rt
            .block_on(
                transport
                    .client()
                    .expect("guarded test client")
                    .get(format!("ftp://{addr}/health"))
                    .send(),
            )
            .expect_err("FTP must not be accepted by the HTTP transport");
        let message = full_error_chain(&err);
        assert!(err.is_builder(), "expected scheme rejection: {message}");
        assert!(
            message.to_lowercase().contains("scheme"),
            "expected an unsupported-scheme diagnostic: {message}"
        );
        assert_eq!(
            listener.accept().expect_err("FTP must not connect").kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    /// Bind an ephemeral-port mock hub that answers exactly one request with
    /// a fixed body, then stops. Good enough for these single-call tests.
    fn spawn_one_shot_mock(
        status: u16,
        body: &'static str,
    ) -> (String, std::thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
        let addr = listener.local_addr().expect("mock local addr");
        let handle = std::thread::spawn(move || -> String {
            let mut stream = accept_bounded(&listener);
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            let mut content_length = 0_usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let trimmed = line.trim_end();
                if trimmed.is_empty() {
                    break;
                }
                if let Some((name, value)) = trimmed.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut request_body = vec![0_u8; content_length];
            let _ = reader.read_exact(&mut request_body);
            let response = format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            String::from_utf8_lossy(&request_body).into_owned()
        });
        (format!("http://{addr}"), handle)
    }

    fn transport_with_token(token: Option<&str>, url: &str) -> HttpMcpTransport {
        let mut settings = isolated_settings();
        settings.auth_token = token.map(str::to_owned);
        settings.server_urls = vec![url.to_owned()];
        HttpMcpTransport::new(&settings)
    }

    #[test]
    fn probe_health_reads_build_version_from_a_real_http_response() {
        let (url, handle) = spawn_one_shot_mock(
            200,
            r#"{"ok": true, "database_ok": true, "storage_ready": true, "build_version": "0.5.0 (75ec8f8)"}"#,
        );
        let transport = transport_with_token(None, &url);
        let info = transport
            .probe_health(&url)
            .expect("healthy response must probe ok");
        assert_eq!(info.build_version.as_deref(), Some("0.5.0 (75ec8f8)"));
        handle.join().expect("mock thread must not panic");
    }

    #[test]
    fn probe_health_returns_none_on_non_2xx() {
        let (url, handle) = spawn_one_shot_mock(500, r#"{"ok": false}"#);
        let transport = transport_with_token(None, &url);
        assert!(transport.probe_health(&url).is_none());
        handle.join().expect("mock thread must not panic");
    }

    #[test]
    fn call_tool_sends_a_tools_call_json_rpc_body_and_unwraps_the_result() {
        let (url, handle) = spawn_one_shot_mock(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"ok\":true,\"database_ok\":true,\"storage_ready\":true,\"build_version\":\"0.5.0 (75ec8f8)\"}"}]}}"#,
        );
        let transport = transport_with_token(Some("secret-token"), &url);
        let value = transport
            .call_tool(&url, "bus_health", &Map::new())
            .expect("call_tool must unwrap the nested JSON-RPC text result");
        assert_eq!(value["build_version"], "0.5.0 (75ec8f8)");
        assert_eq!(value["ok"], true);

        let sent = handle.join().expect("mock thread must not panic");
        assert!(
            sent.contains("\"method\":\"tools/call\""),
            "sent body: {sent}"
        );
        assert!(
            sent.contains("\"name\":\"bus_health\""),
            "sent body: {sent}"
        );
    }

    #[test]
    fn call_tool_attaches_the_bearer_token_from_settings_not_an_inline_argument() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock listener");
        let addr = listener.local_addr().expect("mock local addr");
        let handle = std::thread::spawn(move || -> String {
            let mut stream = accept_bounded(&listener);
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if line.trim().is_empty() {
                    break;
                }
                headers.push_str(&line);
            }
            // Drain and discard the body; the request line/headers above are
            // what this test asserts on.
            let body =
                r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{}"}]}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            headers
        });
        let transport = transport_with_token(Some("secret-token"), &format!("http://{addr}"));
        let _ = transport.call_tool(&format!("http://{addr}"), "bus_health", &Map::new());
        let headers = handle.join().expect("mock thread must not panic");
        assert!(
            headers
                .to_lowercase()
                .contains("authorization: bearer secret-token"),
            "expected bearer auth header from Settings, got headers: {headers}"
        );
    }
}
