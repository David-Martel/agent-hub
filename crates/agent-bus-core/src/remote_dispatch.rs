//! Remote-aware MCP tool dispatch (#78): a thin router in front of
//! [`McpToolDispatch`] that resolves the configured hub candidates
//! ([`Settings::server_urls`]) before every call and either proxies to the
//! resolved remote hub, dispatches locally, or refuses loudly when offline.
//!
//! This module owns no transport of its own — it is generic over
//! [`RemoteMcpTransport`], implemented by each binary crate against whatever
//! HTTP client it already links (keeping `agent-bus-core` free of a hard
//! `reqwest` dependency and making the routing logic itself testable with a
//! fake transport, no network or mock server required).
//!
//! **A hub never proxies to itself.** `agent-bus-http`'s own `/mcp` endpoint
//! must keep constructing [`McpToolDispatch`] directly (see
//! `dispatch_mcp_method` in `agent-bus-http`), never [`RoutingDispatch`] —
//! even if the hub's own `config.json` somehow named a `server_url`, it must
//! never call back out to itself. Only client-facing transports (stdio MCP
//! today) should use [`RoutingDispatch`].

use serde_json::{Map, Value};

use crate::error::{AgentBusError, Result};
use crate::hub::{HubBackend, ProbeInfo, resolve_hub};
use crate::mcp_dispatch::McpToolDispatch;
use crate::ops::admin::health as ops_health;
use crate::settings::Settings;

/// The transport a [`RoutingDispatch`] uses to reach a remote hub.
///
/// Implementations live in the binary crates (e.g. a blocking `reqwest`
/// client in `agent-bus-mcp`), keeping `agent-bus-core` transport-agnostic.
pub trait RemoteMcpTransport {
    /// Probe `{url}/health`. Return `Some` on any healthy 2xx JSON response,
    /// `None` on any failure (unreachable, timeout, non-2xx, malformed
    /// body) — this trait does not distinguish *why* a probe failed.
    fn probe_health(&self, url: &str) -> Option<ProbeInfo>;

    /// Forward a `tools/call` to the hub's `/mcp` JSON-RPC bridge and return
    /// the tool's JSON result value.
    ///
    /// # Errors
    /// Returns an error if the request fails, the hub returns a transport or
    /// JSON-RPC error, or the response cannot be parsed.
    fn call_tool(&self, url: &str, name: &str, args: &Map<String, Value>) -> Result<Value>;
}

/// Routes MCP tool calls to the configured remote hub when reachable, dispatches
/// locally when no hub is configured, and refuses writes/reads loudly when the
/// hub is configured but unreachable — never silently falling back to a local
/// store and presenting it as fleet state.
#[derive(Debug)]
pub struct RoutingDispatch<'a, T> {
    settings: &'a Settings,
    local: McpToolDispatch<'a>,
    transport: T,
}

impl<'a, T: RemoteMcpTransport> RoutingDispatch<'a, T> {
    #[must_use]
    pub fn new(settings: &'a Settings, transport: T) -> Self {
        Self {
            settings,
            local: McpToolDispatch::new(settings),
            transport,
        }
    }

    /// Resolve which backend this call would use right now. Re-resolved on
    /// every call (candidates rarely change within a process lifetime, and a
    /// short-lived stdio session should notice a hub coming back online).
    #[must_use]
    pub fn resolve_backend(&self) -> HubBackend {
        resolve_hub(&self.settings.server_urls, |url| {
            self.transport.probe_health(url)
        })
    }

    /// Dispatch a tool call by name and arguments, returning the result as JSON.
    ///
    /// `bus_health` is always answered (never returns `Err`), with a
    /// `backend` field describing exactly which store answered — remote
    /// (with the hub's build and whether it was the authoritative,
    /// first-priority candidate or a fallback), offline (with every
    /// candidate tried), or local. This is what makes an island detectable:
    /// a client that mistakes a local store for the fleet will show
    /// `backend.mode != "remote"` in its own health report.
    ///
    /// Every other tool: proxied to the resolved remote hub when one
    /// answered, dispatched against the local store when no hub is
    /// configured, or refused with a loud, actionable error when hub
    /// candidates are configured but none answered (never a silent local
    /// fallback).
    ///
    /// # Errors
    /// Returns an error if the tool name is unknown, arguments are invalid,
    /// the resolved remote hub call fails, or every configured hub candidate
    /// is unreachable.
    pub fn dispatch_tool(&self, name: &str, args: &Map<String, Value>) -> Result<Value> {
        if name == "bus_health" {
            return Ok(self.bus_health_report());
        }

        match self.resolve_backend() {
            HubBackend::Local => self.local.dispatch_tool(name, args),
            HubBackend::Remote { url, .. } => self.transport.call_tool(&url, name, args),
            HubBackend::Offline { tried } => Err(AgentBusError::Internal(format!(
                "offline: no authoritative hub reachable (tried {tried:?}); refusing '{name}' \
                 rather than silently using a local store. This client is configured with \
                 remote hub candidates and has no local bus of its own."
            ))),
        }
    }

    fn bus_health_report(&self) -> Value {
        let backend = self.resolve_backend();
        let mut report = match &backend {
            HubBackend::Remote { url, .. } => self
                .transport
                .call_tool(url, "bus_health", &Map::new())
                .unwrap_or_else(|e| {
                    serde_json::json!({
                        "ok": false,
                        "database_ok": false,
                        "storage_ready": false,
                        "error": e.to_string(),
                    })
                }),
            HubBackend::Local => {
                serde_json::to_value(ops_health(self.settings, None)).unwrap_or_default()
            }
            HubBackend::Offline { .. } => serde_json::json!({
                "ok": false,
                "database_ok": false,
                "storage_ready": false,
            }),
        };
        if let Value::Object(ref mut map) = report {
            map.insert(
                "backend".to_owned(),
                serde_json::to_value(&backend).unwrap_or(Value::Null),
            );
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A fake transport recording every call for assertions, with
    /// caller-scripted probe/call outcomes per URL/tool.
    #[derive(Default)]
    struct FakeTransport {
        healthy_urls: Vec<String>,
        hub_build: Option<String>,
        remote_tool_result: RefCell<Option<Value>>,
        remote_tool_error: bool,
        probed: RefCell<Vec<String>>,
        called: RefCell<Vec<(String, String)>>,
    }

    impl RemoteMcpTransport for FakeTransport {
        fn probe_health(&self, url: &str) -> Option<ProbeInfo> {
            self.probed.borrow_mut().push(url.to_owned());
            self.healthy_urls
                .contains(&url.to_owned())
                .then(|| ProbeInfo {
                    build_version: self.hub_build.clone(),
                })
        }

        fn call_tool(&self, url: &str, name: &str, _args: &Map<String, Value>) -> Result<Value> {
            self.called
                .borrow_mut()
                .push((url.to_owned(), name.to_owned()));
            if self.remote_tool_error {
                return Err(AgentBusError::Internal(
                    "remote tool call failed".to_owned(),
                ));
            }
            Ok(self
                .remote_tool_result
                .borrow()
                .clone()
                .unwrap_or_else(|| serde_json::json!({"ok": true, "remote": true})))
        }
    }

    fn test_settings(server_urls: &[String]) -> Settings {
        let mut settings = Settings::from_env();
        settings.redis_url = "redis://127.0.0.1:1/0".to_owned();
        settings.database_url = Some("postgresql://postgres@127.0.0.1:1/none".to_owned());
        settings.server_urls = server_urls.to_owned();
        settings.server_url = server_urls.first().cloned();
        settings
    }

    #[test]
    fn no_candidates_dispatches_locally() {
        let settings = test_settings(&[]);
        let transport = FakeTransport::default();
        let dispatch = RoutingDispatch::new(&settings, transport);
        assert!(matches!(dispatch.resolve_backend(), HubBackend::Local));
        // A non-health tool against the closed-port local backend fails at
        // connect, but it must be a LOCAL connect failure, not the offline
        // error — proving no remote/offline path was taken.
        let err = dispatch
            .dispatch_tool("list_messages", &Map::new())
            .expect_err("closed-port Redis must fail");
        assert!(
            !err.to_string()
                .contains("offline: no authoritative hub reachable"),
            "local mode must not report the offline error: {err}"
        );
    }

    #[test]
    fn reachable_first_candidate_is_authoritative_and_proxied() {
        let settings = test_settings(&["http://a:8400".to_owned(), "http://b:8400".to_owned()]);
        let transport = FakeTransport {
            healthy_urls: vec!["http://a:8400".to_owned()],
            ..Default::default()
        };
        let dispatch = RoutingDispatch::new(&settings, transport);
        let result = dispatch
            .dispatch_tool("list_messages", &Map::new())
            .expect("remote proxy call succeeds");
        assert_eq!(result, serde_json::json!({"ok": true, "remote": true}));
        assert_eq!(
            dispatch.transport.called.borrow().as_slice(),
            [("http://a:8400".to_owned(), "list_messages".to_owned())]
        );
    }

    #[test]
    fn dead_first_candidate_falls_over_to_second_as_non_authoritative() {
        let settings = test_settings(&["http://dead:8400".to_owned(), "http://b:8400".to_owned()]);
        let transport = FakeTransport {
            healthy_urls: vec!["http://b:8400".to_owned()],
            ..Default::default()
        };
        let dispatch = RoutingDispatch::new(&settings, transport);
        match dispatch.resolve_backend() {
            HubBackend::Remote {
                url, authoritative, ..
            } => {
                assert_eq!(url, "http://b:8400");
                assert!(
                    !authoritative,
                    "second candidate must not claim authoritative"
                );
            }
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    #[test]
    fn all_candidates_unreachable_refuses_loudly_without_local_fallback() {
        let settings = test_settings(&[
            "http://dead-1:8400".to_owned(),
            "http://dead-2:8400".to_owned(),
        ]);
        let transport = FakeTransport::default();
        let dispatch = RoutingDispatch::new(&settings, transport);
        let err = dispatch
            .dispatch_tool("post_message", &Map::new())
            .expect_err("offline must refuse, not fall back");
        let message = err.to_string();
        assert!(message.contains("offline: no authoritative hub reachable"));
        assert!(message.contains("dead-1"));
        assert!(message.contains("dead-2"));
        // The fake transport's call_tool must never have been invoked:
        assert!(dispatch.transport.called.borrow().is_empty());
    }

    #[test]
    fn bus_health_reports_remote_backend_with_hub_build() {
        let settings = test_settings(&["http://a:8400".to_owned()]);
        let transport = FakeTransport {
            healthy_urls: vec!["http://a:8400".to_owned()],
            hub_build: Some("0.5.0 (75ec8f8)".to_owned()),
            remote_tool_result: RefCell::new(Some(serde_json::json!({"ok": true}))),
            ..Default::default()
        };
        let dispatch = RoutingDispatch::new(&settings, transport);
        let report = dispatch
            .dispatch_tool("bus_health", &Map::new())
            .expect("bus_health never errors");
        assert_eq!(report["backend"]["mode"], "remote");
        assert_eq!(report["backend"]["url"], "http://a:8400");
        assert_eq!(report["backend"]["authoritative"], true);
        assert_eq!(report["backend"]["hub_build"], "0.5.0 (75ec8f8)");
    }

    #[test]
    fn bus_health_reports_offline_without_erroring_and_without_local_data() {
        let settings = test_settings(&["http://dead:8400".to_owned()]);
        let transport = FakeTransport::default();
        let dispatch = RoutingDispatch::new(&settings, transport);
        let report = dispatch
            .dispatch_tool("bus_health", &Map::new())
            .expect("bus_health never errors");
        assert_eq!(report["backend"]["mode"], "offline");
        assert_eq!(
            report["backend"]["tried"],
            serde_json::json!(["http://dead:8400"])
        );
        assert_eq!(report["ok"], false);
    }

    #[test]
    fn bus_health_reports_local_mode_when_no_candidates_configured() {
        let settings = test_settings(&[]);
        let transport = FakeTransport::default();
        let dispatch = RoutingDispatch::new(&settings, transport);
        let report = dispatch
            .dispatch_tool("bus_health", &Map::new())
            .expect("bus_health never errors");
        assert_eq!(report["backend"]["mode"], "local");
    }

    /// Negative control: proves the previous (pre-#78) always-local behavior
    /// really would have failed this test. `McpToolDispatch` alone (what
    /// every MCP dispatch call used before this change) has no concept of
    /// `server_urls` at all, so it always reports its own local Redis/PG
    /// health regardless of configured remote candidates — it would show
    /// `backend` missing entirely (or, if merged naively, `mode: "local"`
    /// even when candidates are configured), which is exactly the
    /// undetectable-island failure #78 reports.
    #[test]
    fn negative_control_bare_mcp_tool_dispatch_has_no_backend_field() {
        let settings = test_settings(&["http://a:8400".to_owned()]);
        let bare = McpToolDispatch::new(&settings);
        let health = bare
            .dispatch_tool("bus_health", &Map::new())
            .expect("local ops_health always answers");
        assert!(
            health.get("backend").is_none(),
            "pre-#78 dispatch has no backend field to distinguish local from remote"
        );
    }
}
