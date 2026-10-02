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

/// Process-wide `reqwest::Client`, built once and cheaply cloned by every
/// [`HttpMcpTransport`]. Mirrors `agent-bus-cli`'s `server_mode::SERVER_CLIENT`
/// pattern (#78 review item 6): `mcp.rs`'s `call_tool_now` constructs a fresh
/// `HttpMcpTransport` on every MCP tool call, and `reqwest::Client` is
/// `Clone` + internally `Arc`'d (connection pool, TLS config), so sharing one
/// avoids rebuilding the TLS stack and a fresh connection pool per call.
///
/// Deliberately does NOT carry the bearer token (unlike `SERVER_CLIENT`,
/// which bakes it into default headers): [`HttpMcpTransport`] keeps
/// `auth_token` per-instance and attaches it per-request via
/// [`HttpMcpTransport::authed`], because tests in this module construct
/// transports with different tokens in the same process — baking the token
/// into the shared client would make the first-constructed token "stick"
/// for every later instance.
static SHARED_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn shared_client() -> reqwest::Client {
    SHARED_CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(CONNECT_TIMEOUT)
                .timeout(REQUEST_TIMEOUT)
                .build()
                .unwrap_or_else(|_| reqwest::Client::new())
        })
        .clone()
}

/// Reqwest-backed [`RemoteMcpTransport`] for the stdio MCP server.
pub(crate) struct HttpMcpTransport {
    client: reqwest::Client,
    auth_token: Option<String>,
}

impl HttpMcpTransport {
    pub(crate) fn new(settings: &Settings) -> Self {
        Self {
            client: shared_client(),
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
        let transport = transport_with_token(None);
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
                        .client
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
        let transport = transport_with_token(None);
        let rt = tokio::runtime::Runtime::new().expect("build test runtime");
        let err = rt
            .block_on(transport.client.get(format!("ftp://{addr}/health")).send())
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
            let (mut stream, _) = listener.accept().expect("accept mock connection");
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

    fn transport_with_token(token: Option<&str>) -> HttpMcpTransport {
        let mut settings = Settings::from_env();
        settings.auth_token = token.map(str::to_owned);
        HttpMcpTransport::new(&settings)
    }

    #[test]
    fn probe_health_reads_build_version_from_a_real_http_response() {
        let (url, handle) = spawn_one_shot_mock(
            200,
            r#"{"ok": true, "database_ok": true, "storage_ready": true, "build_version": "0.5.0 (75ec8f8)"}"#,
        );
        let transport = transport_with_token(None);
        let info = transport
            .probe_health(&url)
            .expect("healthy response must probe ok");
        assert_eq!(info.build_version.as_deref(), Some("0.5.0 (75ec8f8)"));
        handle.join().expect("mock thread must not panic");
    }

    #[test]
    fn probe_health_returns_none_on_non_2xx() {
        let (url, handle) = spawn_one_shot_mock(500, r#"{"ok": false}"#);
        let transport = transport_with_token(None);
        assert!(transport.probe_health(&url).is_none());
        handle.join().expect("mock thread must not panic");
    }

    #[test]
    fn call_tool_sends_a_tools_call_json_rpc_body_and_unwraps_the_result() {
        let (url, handle) = spawn_one_shot_mock(
            200,
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"{\"ok\":true,\"database_ok\":true,\"storage_ready\":true,\"build_version\":\"0.5.0 (75ec8f8)\"}"}]}}"#,
        );
        let transport = transport_with_token(Some("secret-token"));
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
            let (mut stream, _) = listener.accept().expect("accept mock connection");
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
        let transport = transport_with_token(Some("secret-token"));
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
