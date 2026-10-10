//! Remote-aware MCP tool dispatch (#78): a thin router in front of
//! [`McpToolDispatch`] that resolves the configured hub candidates
//! ([`Settings::server_urls`]) before every call and either proxies to the
//! resolved remote hub, dispatches locally, or refuses loudly when offline.
//!
//! This module owns no transport of its own â€” it is generic over
//! [`RemoteMcpTransport`], implemented by each binary crate against whatever
//! HTTP client it already links (keeping `agent-bus-core` free of a hard
//! `reqwest` dependency and making the routing logic itself testable with a
//! fake transport, no network or mock server required).
//!
//! **A hub never proxies to itself.** `agent-bus-http`'s own `/mcp` endpoint
//! must keep constructing [`McpToolDispatch`] directly (see
//! `dispatch_mcp_method` in `agent-bus-http`), never [`RoutingDispatch`] â€”
//! even if the hub's own `config.json` somehow named a `server_url`, it must
//! never call back out to itself. Only client-facing transports (stdio MCP
//! today) should use [`RoutingDispatch`].

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Map, Value};

use crate::error::{AgentBusError, Result};
use crate::hub::{
    HubBackend, ProbeInfo, invalidate_configured_cache, resolve_authoritative_hub,
    resolve_configured_hub,
};
use crate::mcp_dispatch::McpToolDispatch;
use crate::ops::admin::health as ops_health;
use crate::settings::Settings;

/// How long a resolved backend is trusted before [`RoutingDispatch`]
/// re-probes every candidate (operator requirement: "cache the last good one
/// briefly" so a long-lived stdio session does not re-probe -- and, when
/// offline, re-wait out every candidate's connect timeout -- on every single
/// tool call).
const HUB_CACHE_TTL: Duration = Duration::from_secs(15);

/// Process-wide cache of the last resolved [`HubBackend`] per candidate list,
/// since [`RoutingDispatch`] is constructed fresh on every MCP tool call (see
/// `agent-bus-mcp`'s `call_tool_now`) and has nowhere longer-lived of its own
/// to hold state. Keyed by the candidate list itself (rather than assuming
/// one fixed list per process) so a settings change is never served a stale
/// entry, and so unit tests using different candidate lists never see each
/// other's cached results.
type HubCacheEntry = (HubBackend, Instant);
type HubCacheMap = HashMap<String, HubCacheEntry>;

static HUB_CACHE: OnceLock<Mutex<HubCacheMap>> = OnceLock::new();

fn hub_cache() -> &'static Mutex<HubCacheMap> {
    HUB_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Discard the cached entry for `candidates` so the next
/// [`RoutingDispatch::resolve_backend`] call re-probes every candidate from
/// scratch. Called whenever a call against the cached remote hub actually
/// fails -- a briefly-cached "it was reachable 10 seconds ago" must not
/// survive a live proof that it currently is not.
fn invalidate_hub_cache(settings: &Settings) {
    if let Ok(mut guard) = hub_cache().lock() {
        guard.remove(&crate::hub_cache::candidates_fingerprint(
            &settings.effective_hub_candidates(),
        ));
    }
}

/// MCP tool names that grant, renew, release or resolve an exclusive claim.
/// These must go only to the authoritative hub candidate (see
/// [`RoutingDispatch::dispatch_tool`]) -- mirrors the CLI's own
/// `resolve_authoritative_claim_url` gate in `agent-bus-cli`'s
/// `server_mode.rs`.
fn is_claim_authority_tool(name: &str) -> bool {
    matches!(
        name,
        "claim_resource" | "renew_claim" | "release_claim" | "resolve_claim"
    )
}

/// The operator's exact "claim pending" wording, used both when no candidate
/// answered at all and when a candidate answered but was not the
/// authoritative one -- a caller polling for the claim should retry either
/// way, not distinguish the two.
fn claim_pending_error(name: &str, tried: &[String]) -> AgentBusError {
    AgentBusError::Internal(format!(
        "{name}: claim pending: no authoritative hub reachable (tried {tried:?}); exclusive \
         claims cannot be granted, renewed, released or resolved against a non-authoritative \
         fallback or while offline."
    ))
}

/// The transport a [`RoutingDispatch`] uses to reach a remote hub.
///
/// Implementations live in the binary crates (e.g. a blocking `reqwest`
/// client in `agent-bus-mcp`), keeping `agent-bus-core` transport-agnostic.
pub trait RemoteMcpTransport {
    /// Probe `{url}/health`. Return `Some` on any healthy 2xx JSON response,
    /// `None` on any failure (unreachable, timeout, non-2xx, malformed
    /// body) â€” this trait does not distinguish *why* a probe failed.
    fn probe_health(&self, url: &str) -> Option<ProbeInfo>;

    /// Forward a `tools/call` to the hub's `/mcp` JSON-RPC bridge and return
    /// the tool's JSON result value.
    ///
    /// # Errors
    /// Returns an error if the request fails, the hub returns a transport or
    /// JSON-RPC error, or the response cannot be parsed.
    fn call_tool(&self, url: &str, name: &str, args: &Map<String, Value>) -> Result<Value>;

    /// Only native transports with reviewed replay semantics opt in.
    fn supports_durable_replay(&self) -> bool {
        false
    }

    /// Separate immutable replay envelope; no extra MCP tool is advertised.
    ///
    /// # Errors
    /// Transport ambiguity or permanent rejection of the immutable request.
    fn replay_request(
        &self,
        _url: &str,
        _hub: &str,
        _request: &crate::outbox::ReplayRequest,
        _timeout: Duration,
    ) -> std::result::Result<crate::outbox::ReplayResponse, crate::outbox::ReplayFailure> {
        Err(crate::outbox::ReplayFailure::Permanent)
    }
}

struct DurableTransport<'a, T>(&'a T);
impl<T: RemoteMcpTransport> crate::outbox_client::NativeReplayTransport
    for DurableTransport<'_, T>
{
    fn replay(
        &self,
        url: &str,
        hub: &str,
        request: &crate::outbox::ReplayRequest,
        timeout: Duration,
    ) -> std::result::Result<crate::outbox::ReplayResponse, crate::outbox::ReplayFailure> {
        self.0.replay_request(url, hub, request, timeout)
    }
}
/// Routes MCP tool calls to the configured remote hub when reachable, dispatches
/// locally when no hub is configured, and refuses writes/reads loudly when the
/// hub is configured but unreachable â€” never silently falling back to a local
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

    /// Resolve which backend this call would use right now.
    ///
    /// Cached briefly ([`HUB_CACHE_TTL`]) so a long-lived stdio session does
    /// not re-probe every candidate (including waiting out each dead one's
    /// connect timeout when offline) on every single tool call. The cache is
    /// invalidated immediately whenever an actual call against the cached
    /// remote hub fails (see [`Self::dispatch_tool`]), so a briefly-stale
    /// "it was reachable a few seconds ago" can never survive a live proof
    /// that it currently is not.
    #[must_use]
    pub fn resolve_backend(&self) -> HubBackend {
        let candidates =
            crate::hub_cache::candidates_fingerprint(&self.settings.effective_hub_candidates());
        if self.settings.hub_config_error.is_some() {
            return HubBackend::Offline {
                tried: self.settings.server_urls.clone(),
            };
        }
        if let Ok(guard) = hub_cache().lock()
            && let Some((backend, at)) = guard.get(&candidates)
            && at.elapsed() < HUB_CACHE_TTL
        {
            return backend.clone();
        }

        let backend = resolve_configured_hub(self.settings, |url| self.transport.probe_health(url));
        if let Ok(mut guard) = hub_cache().lock() {
            guard.insert(candidates, (backend.clone(), Instant::now()));
        }
        backend
    }

    /// Dispatch a tool call by name and arguments, returning the result as JSON.
    ///
    /// `bus_health` is always answered (never returns `Err`), with a
    /// `backend` field describing exactly which store answered â€” remote
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
        if let Some(error) = &self.settings.hub_config_error {
            return Err(AgentBusError::InvalidParams(error.clone()));
        }
        // Reconnect/read calls drain existing IDs without creating a new entry.
        // Invalid tools never cause unrelated writes. The health result retains
        // its existing backend contract and exposes any journal drain failure.
        let automatic_drain = if self.transport.supports_durable_replay()
            && crate::outbox::Operation::from_tool(name).is_none()
            && self
                .settings
                .effective_hub_candidates()
                .iter()
                .any(|candidate| {
                    candidate.role == crate::hub_candidates::HubRole::Authoritative
                        && candidate.hub.as_ref().is_some_and(|hub| !hub.is_empty())
                }) {
            crate::mcp_dispatch::validate_tool_arguments(name, args)?;
            Some(crate::outbox_client::flush_pending(
                self.settings,
                &DurableTransport(&self.transport),
                None,
            ))
        } else {
            None
        };
        if name == "bus_health" {
            let mut result = self.bus_health_report();
            if let Some(drain) = automatic_drain {
                result["outbox"] = match drain {
                    Ok(report) => report,
                    Err(_) => {
                        serde_json::json!({"status":"pending_recovery","error":"durable outbox unavailable; preserve journal"})
                    }
                };
            }
            return Ok(result);
        }
        if let Some(drain) = automatic_drain {
            let _ = drain?;
        }
        // Four original #80 operations use stable-ID native replay. Legacy
        // unnamed candidates retain their existing path; they cannot safely
        // bind an offline queue to an unknown hub identity.
        if self.transport.supports_durable_replay()
            && crate::outbox_client::durable_replay_configured(self.settings)
            && let Some(operation) = crate::outbox::Operation::from_tool(name)
        {
            return crate::outbox_client::submit(
                self.settings,
                &DurableTransport(&self.transport),
                operation,
                crate::outbox::ClientSurface::Mcp,
                args.clone(),
                None,
            );
        }

        if self.transport.supports_durable_replay()
            && crate::outbox::Operation::from_tool(name).is_some()
            && !crate::outbox_client::durable_replay_configured(self.settings)
        {
            crate::outbox_client::legacy_write_guard(self.settings, None)?;
        }

        // Claim-authority tools (grant/renew/release/resolve an exclusive
        // claim) must go ONLY to the authoritative (first-priority)
        // candidate -- never a reachable-but-lower-priority fallback, and
        // never offline. Once a second, later hub tier exists (e.g. a future
        // Cloudflare-hosted candidate), there must be exactly one claims
        // authority, so both "nothing answered" and "something answered but
        // it wasn't the authoritative one" get the same retryable "claim
        // pending" answer, distinct from the generic offline error other
        // tools get.
        if is_claim_authority_tool(name) {
            return match resolve_authoritative_hub(self.settings, |url| {
                self.transport.probe_health(url)
            }) {
                HubBackend::Remote {
                    url,
                    authoritative: true,
                    ..
                } => self.transport.call_tool(&url, name, args).inspect_err(|_| {
                    invalidate_hub_cache(self.settings);
                    invalidate_configured_cache(self.settings);
                }),
                HubBackend::Remote { tried, .. } | HubBackend::Offline { tried } => {
                    Err(claim_pending_error(name, &tried))
                }
                HubBackend::Local => self.local.dispatch_tool(name, args),
            };
        }

        match self.resolve_backend() {
            HubBackend::Local => self.local.dispatch_tool(name, args),
            HubBackend::Remote { url, .. } => {
                self.transport.call_tool(&url, name, args).inspect_err(|_| {
                    // The cached backend just proved itself stale (reachable
                    // moments ago, failing now) -- never let the rest of the
                    // TTL window keep routing calls at it blind.
                    invalidate_hub_cache(self.settings);
                    invalidate_configured_cache(self.settings);
                })
            }
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
                    // Same reasoning as dispatch_tool's Remote arm: a cached
                    // backend that just failed a real call must not survive
                    // the rest of the TTL window.
                    invalidate_hub_cache(self.settings);
                    invalidate_configured_cache(self.settings);
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
            if !backend.is_local() {
                map.insert(
                    "network_locations".to_owned(),
                    serde_json::to_value(crate::hub::current_network_locations(self.settings))
                        .unwrap_or(Value::Null),
                );
                if let Some(error) = &self.settings.network_location_error {
                    map.insert(
                        "network_location_error".to_owned(),
                        Value::String(error.clone()),
                    );
                }
            }
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
        hub_identity: Option<String>,
        durable: bool,
        remote_tool_result: RefCell<Option<Value>>,
        remote_tool_error: bool,
        probed: RefCell<Vec<String>>,
        called: RefCell<Vec<(String, String)>>,
    }

    impl RemoteMcpTransport for FakeTransport {
        fn supports_durable_replay(&self) -> bool {
            self.durable
        }

        fn probe_health(&self, url: &str) -> Option<ProbeInfo> {
            self.probed.borrow_mut().push(url.to_owned());
            self.healthy_urls
                .contains(&url.to_owned())
                .then(|| ProbeInfo {
                    build_version: self.hub_build.clone(),
                    hub_identity: self.hub_identity.clone(),
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
        let mut settings = Settings::for_test();
        settings.redis_url = "redis://127.0.0.1:1/0".to_owned();
        settings.database_url = Some("postgresql://postgres@127.0.0.1:1/none".to_owned());
        settings.server_urls = server_urls.to_owned();
        settings.server_url = server_urls.first().cloned();
        settings
    }

    #[test]
    fn named_legacy_unauthenticated_authority_dispatches_once_without_outbox() {
        use crate::hub_candidates::{CandidateAuth, HubCandidate, HubRole};
        let url = "http://localhost:8484".to_owned();
        let mut settings = test_settings(std::slice::from_ref(&url));
        settings.auth_token = None;
        settings.hub_candidates = vec![HubCandidate {
            url: url.clone(),
            role: HubRole::Authoritative,
            auth: CandidateAuth::Global,
            hub: Some("fixture-authority".to_owned()),
            sites: Vec::new(),
        }];
        let transport = FakeTransport {
            healthy_urls: vec![url.clone()],
            hub_identity: Some("fixture-authority".to_owned()),
            durable: true,
            ..Default::default()
        };
        let dispatch = RoutingDispatch::new(&settings, transport);
        assert!(!crate::outbox_client::durable_replay_configured(&settings));
        let args = serde_json::json!({"agent":"fixture","resource":"owned-fixture"})
            .as_object()
            .unwrap()
            .clone();
        assert!(dispatch.dispatch_tool("claim_resource", &args).is_ok());
        assert_eq!(
            *dispatch.transport.called.borrow(),
            vec![(url.clone(), "claim_resource".to_owned())]
        );
        // Configuration of any credential source for this SAME authority
        // selects durable replay even when the provider is invalid/missing.
        settings.hub_candidates.push(HubCandidate {
            url: "http://localhost:8485".to_owned(),
            role: HubRole::Authoritative,
            auth: CandidateAuth::TokenFile("/missing-owned-outbox-fixture-token".to_owned()),
            hub: Some("fixture-authority".to_owned()),
            sites: Vec::new(),
        });
        settings.server_urls = settings
            .hub_candidates
            .iter()
            .map(|candidate| candidate.url.clone())
            .collect();
        assert!(crate::outbox_client::durable_replay_configured(&settings));
        let dispatch = RoutingDispatch::new(
            &settings,
            FakeTransport {
                durable: true,
                ..Default::default()
            },
        );
        assert!(dispatch.dispatch_tool("claim_resource", &args).is_err());
        assert!(dispatch.transport.called.borrow().is_empty());
        assert!(dispatch.transport.probed.borrow().is_empty());
        settings.hub_candidates.truncate(1);
        settings.server_urls.truncate(1);
        settings.auth_token = Some(String::new());
        assert!(crate::outbox_client::durable_replay_configured(&settings));
        settings.auth_token = None;
        settings.hub_candidates[0].auth =
            CandidateAuth::TokenEnv("OWNED_OUTBOX_MISSING_PROVIDER_FIXTURE".to_owned());
        assert!(crate::outbox_client::durable_replay_configured(&settings));
    }

    #[test]
    fn no_candidates_dispatches_locally() {
        let settings = test_settings(&[]);
        let transport = FakeTransport::default();
        let dispatch = RoutingDispatch::new(&settings, transport);
        assert!(matches!(dispatch.resolve_backend(), HubBackend::Local));
        // A non-health tool against the closed-port local backend fails at
        // connect, but it must be a LOCAL connect failure, not the offline
        // error â€” proving no remote/offline path was taken.
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
    /// health regardless of configured remote candidates â€” it would show
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

    /// Operator requirement: "cache the last good one briefly" -- a
    /// long-lived stdio session must not re-probe every candidate (including
    /// waiting out each dead one's connect timeout) on every single tool
    /// call. Uses a candidate list not reused by any other test in this
    /// module, since the cache is keyed by candidate-list content and is
    /// process-wide.
    #[test]
    fn resolve_backend_is_cached_briefly_and_invalidated_on_call_failure() {
        let candidates = [
            "http://cache-test-a:8400".to_owned(),
            "http://cache-test-b:8400".to_owned(),
        ];
        let settings = test_settings(&candidates);
        let transport = FakeTransport {
            healthy_urls: vec!["http://cache-test-a:8400".to_owned()],
            remote_tool_error: true,
            ..Default::default()
        };
        let dispatch = RoutingDispatch::new(&settings, transport);

        let first = dispatch.resolve_backend();
        assert!(first.is_remote());
        assert_eq!(
            dispatch.transport.probed.borrow().len(),
            1,
            "first call must probe"
        );

        let second = dispatch.resolve_backend();
        assert_eq!(second, first, "cached result must be identical");
        assert_eq!(
            dispatch.transport.probed.borrow().len(),
            1,
            "a second call within the TTL must be served from cache, not re-probe"
        );

        // A real call against the cached (now-failing) hub must invalidate
        // the cache, so the NEXT resolve_backend re-probes rather than
        // trusting a briefly-cached "it worked a moment ago".
        let call_result = dispatch.dispatch_tool("post_message", &Map::new());
        assert!(
            call_result.is_err(),
            "FakeTransport is configured to fail every call_tool"
        );
        assert_eq!(
            dispatch.transport.probed.borrow().len(),
            1,
            "dispatch_tool must use the cached backend, not re-probe, before the call itself fails"
        );

        let third = dispatch.resolve_backend();
        assert_eq!(
            third, first,
            "same candidates/transport still resolve the same way"
        );
        assert_eq!(
            dispatch.transport.probed.borrow().len(),
            2,
            "invalidation-on-failure must force the next resolve_backend to re-probe"
        );
    }
}
