//! Remote-hub discovery and selection (#78).
//!
//! `dtm-p1gen7` is a roaming laptop, often off the LAN with no path to the
//! authoritative hub. Its config must not assume any single hub URL is
//! always reachable, and a client that cannot reach a hub must never
//! silently fall back to treating its own local store as fleet state (that
//! is the "split-brain island" this module exists to prevent).
//!
//! [`Settings::server_urls`](crate::settings::Settings::server_urls) models
//! the remote target as an ORDERED list of candidates (e.g. a fleet p2p
//! name, a LAN name, a tailnet name, and — in the future — an
//! always-reachable cloud URL). [`resolve_hub`] tries each in order and
//! classifies the result into exactly one of three explicit, distinguishable
//! states:
//!
//! - [`HubBackend::Remote`] — a candidate answered. `authoritative` is `true`
//!   only when it was the *first* candidate (index 0); anything else is a
//!   reachable fallback, which callers should still surface distinctly so an
//!   operator can tell "talking to the real hub" from "talking to whatever
//!   answered".
//! - [`HubBackend::Offline`] — one or more candidates were configured but
//!   none answered. Callers must never read this as "use the local store
//!   instead" — that is exactly the silent-island failure mode #78 reports.
//! - [`HubBackend::Local`] — no candidates were configured at all. This is
//!   the explicit, deliberate local-only mode (e.g. running on the hub host
//!   itself), not a fallback.
//!
//! This module does no I/O of its own: callers supply a `probe` so the actual
//! transport (`reqwest`) stays out of `agent-bus-core`, keeping this crate's
//! dependency footprint unchanged and making the selection logic trivially
//! unit-testable with a fake probe.
//!
//! Configured candidates are resolved in two ways over one shared plan
//! ([`crate::link_route`]): [`resolve_configured_hub`] and
//! [`resolve_authoritative_hub`] walk each store's plan with a synchronous
//! probe (deterministic, used by tests and simple callers), while
//! [`resolve_configured_hub_concurrent`] and
//! [`resolve_authoritative_hub_concurrent`] race aliases of the same named store
//! with an [`AsyncProbe`]. Authority primary and deferred routes are exhausted
//! before independent fallback stores; a cached cloud route cannot bypass this
//! barrier. Both apply the same candidate set,
//! credential, identity and authority rules, and both report what they saw in
//! [`HubBackend`]'s `probes`.

use crate::hub_cache::{CacheOs, HubCache, candidates_fingerprint, default_cache_path};
use crate::hub_candidates::{HubAuth, HubCandidate, HubRole, SystemEnv, validate_candidates};
use crate::link_route::{
    AsyncProbe, PlanRequest, PlannedRoute, ProbeFailure, ProbeReport, RaceResult, RoutePlan, Slot,
    SlotOutcome, Winner, plan_routes, race, walk,
};
use crate::network_location::{
    NetworkProbe, SystemNetwork, current_locations, network_fingerprint,
};
use crate::settings::Settings;
use serde::Serialize;
use std::time::{Duration, SystemTime};

/// What a candidate hub's `/health` probe found, when it succeeded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeInfo {
    /// The hub's `build_version` (see [`crate::build_info::BUILD_VERSION`]),
    /// when the probe response carried one.
    pub build_version: Option<String>,
    /// The hub's `hub_identity`, when it reports one. Compared with a
    /// candidate's configured `hub`: a mismatch rejects the route, a missing
    /// value is accepted (older hubs do not report identity).
    pub hub_identity: Option<String>,
}

/// The backend a client is actually talking to, after resolving the
/// candidate list. Always reported explicitly (e.g. in `bus_health`) so a
/// local island is never mistaken for fleet state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum HubBackend {
    /// A candidate hub answered `/health`.
    Remote {
        /// The candidate URL that answered.
        url: String,
        /// `true` when `url` was the first (highest-priority) candidate.
        /// `false` means every higher-priority candidate was tried and
        /// failed, and this is a reachable fallback, not the primary hub.
        authoritative: bool,
        /// The hub's reported build/version string, when available.
        hub_build: Option<String>,
        /// Every candidate URL tried, in plan order (best link class first),
        /// including the one that answered. When candidates are raced it also
        /// holds the ones started before the winner answered, so the winner
        /// is not necessarily last; see `probes` for what each one did.
        tried: Vec<String>,
        /// What routes in evaluated store groups did: their link class and
        /// either healthy round trip or why they did not win. Later independent
        /// groups are not contacted after success. Empty for the plain-string
        /// [`resolve_hub`]. Never carries credentials.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        probes: Vec<ProbeReport>,
    },
    /// One or more candidates were configured but none answered within the
    /// connect timeout. Reads must be labelled `offline`/`stale`, never
    /// treated as authoritative; writes (send, presence, claim, ack) must be
    /// refused loudly rather than silently applied to a local store.
    Offline {
        /// Every candidate URL tried, in plan order.
        tried: Vec<String>,
        /// What every planned candidate did (see [`HubBackend::Remote`]).
        #[serde(skip_serializing_if = "Vec::is_empty")]
        probes: Vec<ProbeReport>,
    },
    /// No candidates were configured: this process IS the local store by
    /// deliberate configuration (e.g. it runs on the hub host itself), not
    /// because a remote hub was unreachable.
    Local,
}

impl HubBackend {
    /// `true` for [`HubBackend::Remote`].
    #[must_use]
    pub const fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }

    /// `true` for [`HubBackend::Offline`].
    #[must_use]
    pub const fn is_offline(&self) -> bool {
        matches!(self, Self::Offline { .. })
    }

    /// `true` for [`HubBackend::Local`].
    #[must_use]
    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local)
    }

    /// The URL currently in use, when [`HubBackend::Remote`].
    #[must_use]
    pub fn url(&self) -> Option<&str> {
        match self {
            Self::Remote { url, .. } => Some(url.as_str()),
            Self::Offline { .. } | Self::Local => None,
        }
    }

    /// The JSON placed under `backend` in `health` output: the serialized
    /// state plus `selected`, the URL chosen (`null` when offline).
    #[must_use]
    pub fn to_report(&self) -> serde_json::Value {
        let mut report = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        if let Some(map) = report.as_object_mut() {
            match self {
                Self::Remote { url, .. } => {
                    map.insert("selected".to_owned(), serde_json::Value::from(url.as_str()));
                }
                Self::Offline { .. } => {
                    map.insert("selected".to_owned(), serde_json::Value::Null);
                }
                Self::Local => {}
            }
        }
        report
    }
}

/// Resolve the active hub backend from an ordered candidate list.
///
/// Tries each candidate in order via `probe`, stopping at the first success.
/// `probe` returns `Some(ProbeInfo)` on a healthy response within its own
/// timeout, `None` on any failure (unreachable, timeout, non-2xx, malformed
/// body) — this function does not interpret *why* a probe failed, only
/// whether it succeeded.
///
/// Pure and synchronous: no I/O happens here, only in `probe`. This is what
/// makes the selection logic (ordering, `authoritative`, offline
/// classification) unit-testable without a network or a mock server.
pub fn resolve_hub(
    candidates: &[String],
    mut probe: impl FnMut(&str) -> Option<ProbeInfo>,
) -> HubBackend {
    if candidates.is_empty() {
        return HubBackend::Local;
    }

    let mut tried = Vec::with_capacity(candidates.len());
    for (index, url) in candidates.iter().enumerate() {
        tried.push(url.clone());
        if let Some(info) = probe(url) {
            return HubBackend::Remote {
                url: url.clone(),
                authoritative: index == 0,
                hub_build: info.build_version,
                tried,
                probes: Vec::new(),
            };
        }
    }
    HubBackend::Offline {
        tried,
        probes: Vec::new(),
    }
}

/// Resolve configured roles and credentials, optionally reusing a last-good URL.
///
/// Walks the [`RoutePlan`] in order with a synchronous `probe` and stops at the
/// first accepted answer. The cached URL is always authenticated and probed
/// again before use. Cache failure never changes authority or permits a
/// local-store fallback.
pub fn resolve_configured_hub(
    settings: &Settings,
    mut probe: impl FnMut(&str) -> Option<ProbeInfo>,
) -> HubBackend {
    resolve_settings_hub(settings, &mut probe, false)
}

/// Probe only the configured claims authority, bypassing cached fallbacks.
pub fn resolve_authoritative_hub(
    settings: &Settings,
    mut probe: impl FnMut(&str) -> Option<ProbeInfo>,
) -> HubBackend {
    resolve_settings_hub(settings, &mut probe, true)
}

/// [`resolve_configured_hub`], but racing the candidates concurrently.
///
/// Probes start at the delays of the [`RoutePlan`] (better link classes first,
/// each worse class one stagger later) and the first healthy, identity-checked
/// answer wins; the rest are cancelled. Dead paths cost nothing beyond their
/// own failure, so an offline-heavy client no longer pays one timeout per
/// candidate.
pub async fn resolve_configured_hub_concurrent(
    settings: &Settings,
    probe: &AsyncProbe,
) -> HubBackend {
    resolve_settings_hub_concurrent(settings, probe, false).await
}

/// [`resolve_authoritative_hub`], racing the authoritative routes concurrently.
pub async fn resolve_authoritative_hub_concurrent(
    settings: &Settings,
    probe: &AsyncProbe,
) -> HubBackend {
    resolve_settings_hub_concurrent(settings, probe, true).await
}

/// Run `future` to completion on the ambient multi-thread Tokio runtime from
/// synchronous code, or return `None` when there is no such runtime (a plain
/// thread, or a current-thread runtime where blocking would deadlock).
pub fn block_on_ambient<F: std::future::Future>(future: F) -> Option<F::Output> {
    let handle = tokio::runtime::Handle::try_current().ok()?;
    (handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread)
        .then(|| tokio::task::block_in_place(|| handle.block_on(future)))
}

fn resolution_cache(settings: &Settings) -> Option<HubCache> {
    default_cache_path(CacheOs::current(), &SystemEnv)
        .map(|path| HubCache::new(path, Duration::from_secs(settings.hub_cache_ttl_seconds)))
}

/// Invalidate a last-good URL after a failed real operation.
pub fn invalidate_configured_cache(settings: &Settings) {
    if let Some(cache) = resolution_cache(settings) {
        cache.invalidate();
    }
}

/// The client's current network locations under `settings`, or `None` when
/// location-aware ordering is off (no `network_locations` rules and no
/// `AGENT_BUS_NETWORK_LOCATION` override).
#[must_use]
pub fn current_network_locations(settings: &Settings) -> Option<Vec<String>> {
    current_locations(&settings.network_locations, &SystemNetwork)
}

/// A candidate that names a `hub` is usable only when the probed hub reports
/// exactly that `hub_identity`. A missing identity is a mismatch: several
/// authoritative routes are valid only because they share one `hub`, so an
/// unverified route (a dead tunnel whose port was reused, an older or
/// unrelated hub) must never become a claims authority. Candidates without a
/// `hub` keep the historical behaviour and accept any healthy hub.
///
/// This is a routing label, not authentication: the probe already carries
/// the candidate's credential, and any process can report any identity.
fn identity_matches(candidate: &HubCandidate, info: &ProbeInfo) -> bool {
    candidate
        .hub
        .as_ref()
        .is_none_or(|expected| info.hub_identity.as_ref() == Some(expected))
}

/// Cache key: the candidate fingerprint, the location set and the local
/// network fingerprint. A last-good route learned at one location, or on one
/// set of network attachments, is never tried first at another; plugging in a
/// Thunderbolt or Ethernet link therefore forces a fresh race.
fn routing_fingerprint(
    candidates: &[HubCandidate],
    locations: Option<&[String]>,
    network: &str,
) -> String {
    let base = candidates_fingerprint(candidates);
    let located = match locations {
        None => base,
        Some(names) => {
            let mut names = names.to_vec();
            names.sort_unstable();
            format!("{base}@{}", names.join(","))
        }
    };
    format!("{located}#{network}")
}

/// The fingerprint of this machine's current network attachments.
fn system_network_fingerprint() -> String {
    network_fingerprint(&SystemNetwork.prefixes())
}

/// Everything one resolution needs, computed once and shared by the serial
/// walk and the concurrent race so both apply identical rules.
struct Resolution {
    candidates: Vec<HubCandidate>,
    plans: Vec<RoutePlan>,
    auth: HubAuth,
    cache: Option<HubCache>,
    fingerprint: String,
}

/// What was attempted so far, for [`HubBackend`]'s `tried` and `probes`.
#[derive(Default)]
struct Tally {
    tried: Vec<String>,
    probes: Vec<ProbeReport>,
}

/// Plan interchangeable routes within one store before considering another store.
///
/// A role and explicit hub identity define a group. Unnamed URLs are separate
/// stores, not evidence of shared state. Location and cache preferences only
/// reorder routes inside a group; neither may promote cloud above authority.
fn grouped_route_plans(
    candidates: &[HubCandidate],
    locations: Option<&[String]>,
    cached: Option<usize>,
    authority_only: bool,
    stagger: Duration,
) -> Vec<RoutePlan> {
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for role in [HubRole::Authoritative, HubRole::Fallback, HubRole::Cloud] {
        if authority_only && role != HubRole::Authoritative {
            continue;
        }
        for (index, candidate) in candidates.iter().enumerate() {
            if candidate.role != role {
                continue;
            }
            let existing = candidate
                .hub
                .as_deref()
                .filter(|hub| !hub.is_empty())
                .and_then(|hub| {
                    groups.iter().position(|group| {
                        let first = &candidates[group[0]];
                        first.role == role && first.hub.as_deref() == Some(hub)
                    })
                });
            if let Some(group) = existing {
                groups[group].push(index);
            } else {
                groups.push(vec![index]);
            }
        }
    }
    groups
        .into_iter()
        .map(|indexes| {
            let members: Vec<_> = indexes.iter().map(|&i| candidates[i].clone()).collect();
            let cached = cached.and_then(|i| indexes.iter().position(|&member| member == i));
            let mut plan = plan_routes(&PlanRequest {
                candidates: &members,
                locations,
                cached,
                authority_only,
                stagger,
            });
            // Auth, diagnostics and acceptance use indexes in the original list.
            for route in plan.primary.iter_mut().chain(&mut plan.deferred) {
                route.index = indexes[route.index];
            }
            plan
        })
        .collect()
}

impl Resolution {
    /// Build the plan, or return the backend to report without probing:
    /// invalid configuration is `Offline` (never `Local`), no candidates is
    /// deliberate `Local`.
    fn begin(
        settings: &Settings,
        authority_only: bool,
        locations: Option<&[String]>,
        network: &str,
        cache: Option<HubCache>,
    ) -> Result<Self, HubBackend> {
        let candidates = settings.effective_hub_candidates();
        if settings.hub_config_error.is_some() || validate_candidates(&candidates).is_err() {
            return Err(HubBackend::Offline {
                tried: settings.server_urls.clone(),
                probes: Vec::new(),
            });
        }
        if candidates.is_empty() {
            return Err(HubBackend::Local);
        }
        let auth = HubAuth::new(settings.auth_token.clone(), candidates.clone());
        let fingerprint = routing_fingerprint(&candidates, locations, network);
        let cached = if authority_only {
            None
        } else {
            cache
                .as_ref()
                .and_then(|cache| cache.load(&fingerprint, SystemTime::now()))
                .and_then(|cached| {
                    candidates.iter().position(|candidate| {
                        candidate.url == cached.url && candidate.role == cached.role
                    })
                })
        };
        let plans = grouped_route_plans(
            &candidates,
            locations,
            cached,
            authority_only,
            Duration::from_millis(settings.probe_stagger_ms),
        );
        Ok(Self {
            candidates,
            plans,
            auth,
            cache,
            fingerprint,
        })
    }

    /// The probes for one stage. A route whose credential cannot be resolved
    /// is skipped without being contacted, so a token is never sent to a
    /// route it was not configured for.
    fn slots(&self, routes: &[PlannedRoute]) -> Vec<Slot> {
        routes
            .iter()
            .map(|route| {
                let candidate = &self.candidates[route.index];
                let skip = self
                    .auth
                    .credential_for(candidate, &SystemEnv)
                    .err()
                    .map(|reason| {
                        tracing::debug!(reason = %reason, "hub candidate credential unavailable");
                        ProbeFailure::CredentialUnavailable
                    });
                Slot {
                    url: candidate.url.clone(),
                    delay: route.delay,
                    skip,
                }
            })
            .collect()
    }

    /// `true` when the healthy answer `info` is acceptable for `route`.
    fn accepts(&self, route: &PlannedRoute, info: &ProbeInfo) -> bool {
        let candidate = &self.candidates[route.index];
        let accepted = identity_matches(candidate, info);
        if !accepted {
            tracing::debug!(
                expected = ?candidate.hub,
                reported = ?info.hub_identity,
                "hub candidate reached a different hub; skipping"
            );
        }
        accepted
    }

    fn record(&self, tally: &mut Tally, routes: &[PlannedRoute], result: &RaceResult) {
        for (route, outcome) in routes.iter().zip(&result.outcomes) {
            let candidate = &self.candidates[route.index];
            let attempted = match outcome {
                SlotOutcome::Healthy { .. } => true,
                SlotOutcome::Failed(reason) => reason.was_attempted(),
            };
            if attempted {
                tally.tried.push(candidate.url.clone());
            }
            tally
                .probes
                .push(ProbeReport::new(&candidate.url, route.class, *outcome));
        }
    }

    /// Remember the winner and report it.
    fn succeed(&self, route: &PlannedRoute, winner: Winner, tally: Tally) -> HubBackend {
        let candidate = &self.candidates[route.index];
        if let Some(cache) = &self.cache
            && let Err(error) = cache.store_with_rtt(
                &self.fingerprint,
                &candidate.url,
                candidate.role,
                u32::try_from(winner.rtt.as_millis()).ok(),
                SystemTime::now(),
            )
        {
            tracing::debug!(kind = ?error.kind(), "hub resolution cache write failed");
        }
        HubBackend::Remote {
            url: candidate.url.clone(),
            authoritative: candidate.role == HubRole::Authoritative,
            hub_build: winner.info.build_version,
            tried: tally.tried,
            probes: tally.probes,
        }
    }

    /// Nothing answered: forget any cached route and report `Offline`.
    fn exhausted(self, tally: Tally) -> HubBackend {
        if let Some(cache) = self.cache {
            cache.invalidate();
        }
        HubBackend::Offline {
            tried: tally.tried,
            probes: tally.probes,
        }
    }
}

fn resolve_settings_hub(
    settings: &Settings,
    probe: &mut impl FnMut(&str) -> Option<ProbeInfo>,
    authority_only: bool,
) -> HubBackend {
    resolve_serial(
        settings,
        probe,
        authority_only,
        current_network_locations(settings).as_deref(),
        &system_network_fingerprint(),
        resolution_cache(settings),
    )
}

/// Walk each store's primary and deferred routes before moving to another store.
fn resolve_serial(
    settings: &Settings,
    probe: &mut impl FnMut(&str) -> Option<ProbeInfo>,
    authority_only: bool,
    locations: Option<&[String]>,
    network: &str,
    cache: Option<HubCache>,
) -> HubBackend {
    let resolution = match Resolution::begin(settings, authority_only, locations, network, cache) {
        Ok(resolution) => resolution,
        Err(backend) => return backend,
    };
    let mut tally = Tally::default();
    for plan in &resolution.plans {
        for routes in [&plan.primary, &plan.deferred] {
            if routes.is_empty() {
                continue;
            }
            let slots = resolution.slots(routes);
            let result = walk(&slots, probe, |slot, info| {
                resolution.accepts(&routes[slot], info)
            });
            resolution.record(&mut tally, routes, &result);
            if let Some(winner) = result.winner {
                return resolution.succeed(&routes[winner.slot], winner, tally);
            }
        }
    }
    resolution.exhausted(tally)
}

async fn resolve_settings_hub_concurrent(
    settings: &Settings,
    probe: &AsyncProbe,
    authority_only: bool,
) -> HubBackend {
    resolve_concurrent(
        settings,
        probe,
        authority_only,
        current_network_locations(settings).as_deref(),
        &system_network_fingerprint(),
        resolution_cache(settings),
    )
    .await
}

/// Race aliases within each store, including its deferred routes before fallback.
async fn resolve_concurrent(
    settings: &Settings,
    probe: &AsyncProbe,
    authority_only: bool,
    locations: Option<&[String]>,
    network: &str,
    cache: Option<HubCache>,
) -> HubBackend {
    let resolution = match Resolution::begin(settings, authority_only, locations, network, cache) {
        Ok(resolution) => resolution,
        Err(backend) => return backend,
    };
    let budget = Duration::from_millis(settings.probe_connect_timeout_ms);
    let mut tally = Tally::default();
    for plan in &resolution.plans {
        for routes in [&plan.primary, &plan.deferred] {
            if routes.is_empty() {
                continue;
            }
            let slots = resolution.slots(routes);
            let result = race(&slots, probe, budget, |slot, info| {
                resolution.accepts(&routes[slot], info)
            })
            .await;
            resolution.record(&mut tally, routes, &result);
            if let Some(winner) = result.winner {
                return resolution.succeed(&routes[winner.slot], winner, tally);
            }
        }
    }
    resolution.exhausted(tally)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serial resolution at a fixed `locations`, with a fixed network and no
    /// on-disk cache, so ordering and acceptance rules stay deterministic.
    fn resolve_settings_hub_at(
        settings: &Settings,
        probe: &mut impl FnMut(&str) -> Option<ProbeInfo>,
        authority_only: bool,
        locations: Option<&[String]>,
    ) -> HubBackend {
        resolve_serial(settings, probe, authority_only, locations, "test-net", None)
    }

    fn configured_settings(urls: &[&str]) -> Settings {
        let mut settings = Settings::from_env();
        settings.server_urls = urls.iter().map(|url| (*url).to_owned()).collect();
        settings.hub_candidates = crate::hub_candidates::build_candidates(
            &settings.server_urls,
            &std::collections::BTreeMap::new(),
        );
        settings.hub_cache_ttl_seconds = 0;
        settings.auth_token = None;
        settings.hub_config_error = None;
        settings
    }

    #[test]
    fn explicit_second_authority_is_used_for_claims() {
        let mut settings =
            configured_settings(&["http://fallback.lan:8400", "http://authority.lan:8400"]);
        settings.hub_candidates[0].role = HubRole::Fallback;
        settings.hub_candidates[1].role = HubRole::Authoritative;
        let mut probed = Vec::new();
        let backend = resolve_authoritative_hub(&settings, |url| {
            probed.push(url.to_owned());
            Some(ProbeInfo::default())
        });
        assert_eq!(probed, vec!["http://authority.lan:8400"]);
        assert!(matches!(
            backend,
            HubBackend::Remote {
                authoritative: true,
                ..
            }
        ));
    }

    #[test]
    fn missing_explicit_credential_skips_probe_instead_of_using_global() {
        let mut settings =
            configured_settings(&["http://missing-token.lan:8400", "http://fallback.lan:8400"]);
        let directory = tempfile::tempdir().expect("temporary credential directory");
        settings.hub_candidates[0].auth = crate::hub_candidates::CandidateAuth::TokenFile(
            directory
                .path()
                .join("absent-token")
                .to_string_lossy()
                .into_owned(),
        );
        settings.auth_token = Some("local-fixture-token".to_owned());
        let mut probed = Vec::new();
        let backend = resolve_configured_hub(&settings, |url| {
            probed.push(url.to_owned());
            Some(ProbeInfo::default())
        });
        assert_eq!(probed, vec!["http://fallback.lan:8400"]);
        assert!(matches!(
            backend,
            HubBackend::Remote {
                authoritative: false,
                ..
            }
        ));
    }

    #[test]
    fn invalid_configuration_never_enters_local_mode_or_probes() {
        let mut settings = configured_settings(&[]);
        settings.hub_config_error = Some("malformed candidates".to_owned());
        assert!(
            resolve_configured_hub(&settings, |_| panic!(
                "invalid config must perform no network request"
            ))
            .is_offline()
        );
    }

    fn urls(values: &[&str]) -> Vec<String> {
        values.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn empty_candidates_is_local() {
        let backend = resolve_hub(&[], |_| panic!("probe must not be called for local mode"));
        assert_eq!(backend, HubBackend::Local);
        assert!(backend.is_local());
        assert!(!backend.is_remote());
        assert!(!backend.is_offline());
        assert_eq!(backend.url(), None);
    }

    /// Protects: legacy first-route authority and selected build reporting.
    /// Detects: wrong authority flag, selected URL or attempted-route report.
    /// Needs: deterministic injected probes without network access.
    /// Breadcrumb: crates/agent-bus-core/src/remote_dispatch.rs (backend consumers).
    #[test]
    fn first_candidate_answering_is_authoritative() {
        let candidates = urls(&["http://a:8400", "http://b:8400"]);
        let backend = resolve_hub(&candidates, |url| {
            assert_eq!(url, "http://a:8400", "must try candidates in order");
            Some(ProbeInfo {
                build_version: Some("0.5.0 (abc123)".to_owned()),
                hub_identity: None,
            })
        });
        assert_eq!(
            backend,
            HubBackend::Remote {
                url: "http://a:8400".to_owned(),
                authoritative: true,
                hub_build: Some("0.5.0 (abc123)".to_owned()),
                tried: urls(&["http://a:8400"]),
                probes: Vec::new(),
            }
        );
        assert!(backend.is_remote());
        assert_eq!(backend.url(), Some("http://a:8400"));
    }

    /// Protects: a later healthy legacy candidate remains a fallback.
    /// Detects: promoting fallback to authority or probing past the winner.
    /// Needs: a synthetic failed first route and healthy second route.
    /// Breadcrumb: crates/agent-bus-core/src/remote_dispatch.rs (backend consumers).
    #[test]
    fn second_candidate_answering_is_a_fallback_not_authoritative() {
        let candidates = urls(&["http://dead:8400", "http://b:8400", "http://c:8400"]);
        let backend = resolve_hub(&candidates, |url| {
            (url == "http://b:8400").then_some(ProbeInfo {
                build_version: None,
                hub_identity: None,
            })
        });
        assert_eq!(
            backend,
            HubBackend::Remote {
                url: "http://b:8400".to_owned(),
                authoritative: false,
                hub_build: None,
                tried: urls(&["http://dead:8400", "http://b:8400"]),
                probes: Vec::new(),
            }
        );
    }

    /// Protects: unreachable configured hubs never select local storage.
    /// Detects: reporting local or remote after all injected probes fail.
    /// Needs: deterministic failed probes and explicit candidate URLs.
    /// Breadcrumb: crates/agent-bus-core/src/remote_dispatch.rs (offline handling).
    #[test]
    fn no_candidate_answering_is_offline_not_local() {
        let candidates = urls(&["http://dead-1:8400", "http://dead-2:8400"]);
        let backend = resolve_hub(&candidates, |_| None);
        assert_eq!(
            backend,
            HubBackend::Offline {
                tried: urls(&["http://dead-1:8400", "http://dead-2:8400"]),
                probes: Vec::new(),
            }
        );
        assert!(backend.is_offline());
        assert!(
            !backend.is_local(),
            "offline must never be reported as local"
        );
        assert!(!backend.is_remote());
        assert_eq!(backend.url(), None);
    }

    #[test]
    fn stops_probing_after_first_success() {
        let candidates = urls(&["http://a:8400", "http://b:8400", "http://c:8400"]);
        let mut calls = Vec::new();
        let backend = resolve_hub(&candidates, |url| {
            calls.push(url.to_owned());
            Some(ProbeInfo {
                build_version: None,
                hub_identity: None,
            })
        });
        assert!(backend.is_remote());
        assert_eq!(
            calls,
            vec!["http://a:8400".to_owned()],
            "must not probe past the first success"
        );
    }

    #[test]
    fn single_candidate_success_is_authoritative() {
        let candidates = urls(&["http://only:8400"]);
        let backend = resolve_hub(&candidates, |_| {
            Some(ProbeInfo {
                build_version: Some("0.5.0".to_owned()),
                hub_identity: None,
            })
        });
        match backend {
            HubBackend::Remote { authoritative, .. } => assert!(authoritative),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

    /// Protects: HTTPS strings reach the injected legacy probe unchanged.
    /// Detects: scheme rejection or wrong remote/offline result shapes.
    /// Needs: synthetic successful/failed probes; no TLS qualification.
    /// Breadcrumb: crates/agent-bus-core/src/remote_dispatch.rs (`probe_health` contract).
    #[test]
    fn https_candidate_is_accepted_by_resolution() {
        // `resolve_hub` does no I/O and no scheme filtering of its own -- it
        // is a plain string handed to `probe`. This proves nothing upstream
        // (this function, or a caller building the candidate list) special-
        // cases or rejects `https://`, which is required for a Cloudflare-
        // hosted `https://agentbus.dtmventures.com` candidate (item 5) to
        // ever reach the actual HTTP client.
        let candidates = urls(&["https://agentbus.dtmventures.com"]);
        let backend = resolve_hub(&candidates, |url| {
            assert_eq!(url, "https://agentbus.dtmventures.com");
            Some(ProbeInfo {
                build_version: Some("0.5.0 (cloud)".to_owned()),
                hub_identity: None,
            })
        });
        assert_eq!(
            backend,
            HubBackend::Remote {
                url: "https://agentbus.dtmventures.com".to_owned(),
                authoritative: true,
                hub_build: Some("0.5.0 (cloud)".to_owned()),
                tried: urls(&["https://agentbus.dtmventures.com"]),
                probes: Vec::new(),
            }
        );

        // Also prove an unreachable https:// candidate lands the ordinary
        // `Offline` state, not some special/rejected variant.
        let candidates = urls(&["https://agentbus.dtmventures.com"]);
        let backend = resolve_hub(&candidates, |_| None);
        assert_eq!(
            backend,
            HubBackend::Offline {
                tried: urls(&["https://agentbus.dtmventures.com"]),
                probes: Vec::new(),
            }
        );
    }

    /// Protects: remote, offline and local JSON mode compatibility.
    /// Detects: missing mode tags or changed authority/build fields.
    /// Needs: in-memory backend values and serde JSON serialization.
    /// Breadcrumb: crates/agent-bus-core/src/remote_dispatch.rs (backend reporting).
    #[test]
    fn serializes_with_a_mode_tag_for_json_reporting() {
        let backend = HubBackend::Remote {
            url: "http://a:8400".to_owned(),
            authoritative: true,
            hub_build: Some("0.5.0 (abc123)".to_owned()),
            tried: urls(&["http://a:8400"]),
            probes: Vec::new(),
        };
        let value = serde_json::to_value(&backend).expect("serializable");
        assert_eq!(value["mode"], "remote");
        assert_eq!(value["url"], "http://a:8400");
        assert_eq!(value["authoritative"], true);
        assert_eq!(value["hub_build"], "0.5.0 (abc123)");

        let offline = HubBackend::Offline {
            tried: urls(&["http://a:8400"]),
            probes: Vec::new(),
        };
        let value = serde_json::to_value(&offline).expect("serializable");
        assert_eq!(value["mode"], "offline");

        let local = HubBackend::Local;
        let value = serde_json::to_value(&local).expect("serializable");
        assert_eq!(value["mode"], "local");
    }

    fn routed_settings() -> Settings {
        let mut settings = configured_settings(&[
            "http://10.0.0.1:8400",
            "http://127.0.0.1:18400",
            "http://192.168.1.9:8400",
        ]);
        settings.hub_candidates[0].sites = vec!["lab-fabric".to_owned()];
        settings.hub_candidates[0].hub = Some("onsite".to_owned());
        settings.hub_candidates[1].role = HubRole::Authoritative;
        settings.hub_candidates[1].sites = vec!["campus".to_owned()];
        settings.hub_candidates[1].hub = Some("onsite".to_owned());
        settings.hub_candidates[2].role = HubRole::Fallback;
        settings
    }

    #[test]
    fn location_puts_the_local_route_first_and_keeps_its_authority() {
        let settings = routed_settings();
        let campus = vec!["campus".to_owned()];
        let mut probed = Vec::new();
        let backend = resolve_settings_hub_at(
            &settings,
            &mut |url: &str| {
                probed.push(url.to_owned());
                (url == "http://127.0.0.1:18400").then(|| ProbeInfo {
                    build_version: None,
                    hub_identity: Some("onsite".to_owned()),
                })
            },
            false,
            Some(&campus),
        );
        assert_eq!(probed, vec!["http://127.0.0.1:18400"]);
        assert!(matches!(
            backend,
            HubBackend::Remote { authoritative: true, ref url, .. } if url == "http://127.0.0.1:18400"
        ));
    }

    #[test]
    fn unknown_location_still_probes_every_declared_route() {
        let settings = routed_settings();
        let mut probed = Vec::new();
        let backend = resolve_settings_hub_at(
            &settings,
            &mut |url: &str| {
                probed.push(url.to_owned());
                None
            },
            false,
            Some(&[]),
        );
        assert_eq!(
            probed,
            vec![
                "http://10.0.0.1:8400",
                "http://127.0.0.1:18400",
                "http://192.168.1.9:8400"
            ],
            "with no matching site the configured order is kept"
        );
        assert!(backend.is_offline());
    }

    #[test]
    fn claims_try_every_route_to_the_authority_in_location_order() {
        let settings = routed_settings();
        let campus = vec!["campus".to_owned()];
        let mut probed = Vec::new();
        let backend = resolve_settings_hub_at(
            &settings,
            &mut |url: &str| {
                probed.push(url.to_owned());
                None
            },
            true,
            Some(&campus),
        );
        assert_eq!(
            probed,
            vec!["http://127.0.0.1:18400", "http://10.0.0.1:8400"]
        );
        assert!(backend.is_offline());
    }

    /// Protects: named authority routes reject a different reported hub.
    /// Detects: accepting a reused tunnel port or hiding identity mismatch.
    /// Needs: synthetic named candidates and a mismatching first answer.
    /// Breadcrumb: crates/agent-bus-core/src/hub_candidates.rs (`HubCandidate` identity).
    #[test]
    fn a_route_reporting_another_hub_identity_is_rejected() {
        // Why: a dead tunnel whose port was reused must not be trusted; the next proven route wins.
        let settings = routed_settings();
        let both = vec!["campus".to_owned(), "lab-fabric".to_owned()];
        let backend = resolve_settings_hub_at(
            &settings,
            &mut |url: &str| {
                Some(ProbeInfo {
                    build_version: None,
                    hub_identity: Some(if url == "http://10.0.0.1:8400" {
                        "someone-else".to_owned()
                    } else {
                        "onsite".to_owned()
                    }),
                })
            },
            false,
            Some(&both),
        );
        // The preferred fabric route is the impostor: it is rejected, and the
        // next route in plan order that proves it is the named hub wins.
        let HubBackend::Remote {
            ref url,
            authoritative,
            ref probes,
            ..
        } = backend
        else {
            panic!("expected a remote backend, got {backend:?}");
        };
        assert_eq!(url, "http://127.0.0.1:18400");
        assert!(authoritative);
        assert_eq!(
            probes[0].error,
            Some(ProbeFailure::IdentityMismatch),
            "the rejected route is reported, not hidden"
        );
    }

    #[test]
    fn a_named_route_requires_the_hub_to_report_its_identity() {
        let settings = routed_settings();
        let fabric = vec!["lab-fabric".to_owned()];
        // Every hub answers without an identity: neither named route may be
        // used, so only the unnamed fallback is reachable.
        let backend = resolve_settings_hub_at(
            &settings,
            &mut |_url: &str| Some(ProbeInfo::default()),
            false,
            Some(&fabric),
        );
        assert!(matches!(
            backend,
            HubBackend::Remote { ref url, authoritative: false, .. } if url == "http://192.168.1.9:8400"
        ));
    }

    #[test]
    fn claims_never_accept_an_unverified_or_foreign_route() {
        let settings = routed_settings();
        let campus = vec!["campus".to_owned()];
        for reported in [None, Some("someone-else".to_owned())] {
            let mut probed = Vec::new();
            let backend = resolve_settings_hub_at(
                &settings,
                &mut |url: &str| {
                    probed.push(url.to_owned());
                    Some(ProbeInfo {
                        build_version: None,
                        hub_identity: reported.clone(),
                    })
                },
                true,
                Some(&campus),
            );
            assert!(backend.is_offline(), "{reported:?} must not grant claims");
            assert_eq!(
                probed,
                vec!["http://127.0.0.1:18400", "http://10.0.0.1:8400"]
            );
        }
    }

    #[test]
    fn candidates_without_a_hub_name_keep_accepting_any_healthy_hub() {
        let settings = configured_settings(&["http://10.0.0.1:8400"]);
        let backend = resolve_settings_hub_at(
            &settings,
            &mut |_url: &str| Some(ProbeInfo::default()),
            true,
            None,
        );
        assert!(matches!(
            backend,
            HubBackend::Remote {
                authoritative: true,
                ..
            }
        ));
    }

    /// Protects: cache keys separate location sets and network attachments.
    /// Detects: location-order sensitivity or attachment key collisions.
    /// Needs: synthetic candidates, location sets and fixed network labels.
    /// Breadcrumb: crates/agent-bus-core/src/network_location.rs (`network_fingerprint`).
    #[test]
    fn routing_fingerprint_separates_locations() {
        let candidates = routed_settings().hub_candidates;
        let unaware = routing_fingerprint(&candidates, None, "net");
        let campus = routing_fingerprint(&candidates, Some(&["campus".to_owned()]), "net");
        let swapped = routing_fingerprint(
            &candidates,
            Some(&["home".to_owned(), "campus".to_owned()]),
            "net",
        );
        let sorted = routing_fingerprint(
            &candidates,
            Some(&["campus".to_owned(), "home".to_owned()]),
            "net",
        );
        assert_ne!(unaware, campus);
        assert_ne!(campus, sorted);
        assert_eq!(swapped, sorted, "location set order is irrelevant");
        let elsewhere = routing_fingerprint(&candidates, Some(&["campus".to_owned()]), "other-net");
        assert_ne!(campus, elsewhere, "a new network attachment is a new key");
    }

    // ---- concurrent resolution against real local sockets ---------------

    use crate::link_route::ProbeFuture;
    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A tiny `/health` server on an ephemeral port that counts connections.
    struct FakeHub {
        url: String,
        hits: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        address: std::net::SocketAddr,
    }

    impl Drop for FakeHub {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _wake = std::net::TcpStream::connect(self.address);
        }
    }

    fn fake_hub(delay: Duration, identity: &'static str) -> FakeHub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake hub");
        let address = listener.local_addr().expect("address");
        let hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (thread_hits, thread_stop) = (Arc::clone(&hits), Arc::clone(&stop));
        std::thread::spawn(move || {
            while let Ok((mut stream, _peer)) = listener.accept() {
                if thread_stop.load(Ordering::SeqCst) {
                    return;
                }
                thread_hits.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let mut buffer = [0_u8; 1024];
                    let _read = stream.read(&mut buffer);
                    std::thread::sleep(delay);
                    let body = format!(r#"{{"ok":true,"hub_identity":"{identity}"}}"#);
                    let _write = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                });
            }
        });
        FakeHub {
            url: format!("http://{address}"),
            hits,
            stop,
            address,
        }
    }

    fn refusing_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let address = listener.local_addr().expect("address");
        drop(listener);
        format!("http://{address}")
    }

    fn socket_probe() -> AsyncProbe {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        Arc::new(|url: String| -> ProbeFuture {
            Box::pin(async move {
                let host = url.strip_prefix("http://")?.to_owned();
                let mut stream = tokio::net::TcpStream::connect(&host).await.ok()?;
                stream
                    .write_all(b"GET /health HTTP/1.1\r\nhost: fake\r\nconnection: close\r\n\r\n")
                    .await
                    .ok()?;
                let mut response = String::new();
                stream.read_to_string(&mut response).await.ok()?;
                let body = response.split("\r\n\r\n").nth(1)?;
                let value: serde_json::Value = serde_json::from_str(body).ok()?;
                Some(ProbeInfo {
                    build_version: None,
                    hub_identity: value["hub_identity"].as_str().map(str::to_owned),
                })
            })
        })
    }

    /// Candidates `[fabric, lan]`, both authoritative routes to hub `hub`.
    fn two_link_settings(fabric: &str, lan: &str) -> Settings {
        let mut settings = configured_settings(&[fabric, lan]);
        settings.hub_candidates[0].sites = vec!["lab-fabric".to_owned()];
        settings.hub_candidates[1].sites = vec!["lab-lan".to_owned()];
        for candidate in &mut settings.hub_candidates {
            candidate.role = HubRole::Authoritative;
            candidate.hub = Some("hub".to_owned());
        }
        settings.probe_connect_timeout_ms = 400;
        settings.probe_stagger_ms = 60;
        settings
    }

    fn both_links() -> Vec<String> {
        vec!["lab-fabric".to_owned(), "lab-lan".to_owned()]
    }

    fn cache_in(dir: &tempfile::TempDir) -> HubCache {
        HubCache::new(
            dir.path().join("hub-resolution.json"),
            Duration::from_secs(600),
        )
    }

    async fn race_at(
        settings: &Settings,
        locations: &[String],
        network: &str,
        cache: Option<HubCache>,
        authority_only: bool,
    ) -> HubBackend {
        resolve_concurrent(
            settings,
            &socket_probe(),
            authority_only,
            Some(locations),
            network,
            cache,
        )
        .await
    }

    /// Protects: a healthy fabric alias wins before its LAN alias starts.
    /// Detects: wrong selected class or an unnecessary LAN socket probe.
    /// Needs: private healthy TCP listeners with explicit shared hub identity.
    /// Breadcrumb: crates/agent-bus-core/src/link_route.rs (`plan_routes` and `race`).
    #[tokio::test]
    async fn the_preferred_link_class_wins_when_both_links_are_healthy() {
        // Why: with a fabric link and a LAN both up, the fabric must be selected.
        let fabric = fake_hub(Duration::ZERO, "hub");
        let lan = fake_hub(Duration::ZERO, "hub");
        let settings = two_link_settings(&fabric.url, &lan.url);
        let backend = race_at(&settings, &both_links(), "net", None, false).await;
        let HubBackend::Remote { url, probes, .. } = &backend else {
            panic!("expected remote, got {backend:?}");
        };
        assert_eq!(url, &fabric.url);
        assert_eq!(probes[0].class, crate::link_route::LinkClass::Fabric);
        assert_eq!(lan.hits.load(Ordering::SeqCst), 0, "the LAN never started");
        assert_eq!(backend.to_report()["selected"], fabric.url);
    }

    /// Protects: same-store aliases race without waiting for a stalled route timeout.
    /// Detects: serial probes or failure to cancel the in-flight losing socket.
    /// Needs: private TCP listeners and matching explicit/legacy candidate lists.
    /// Breadcrumb: crates/agent-bus-core/src/link_route.rs (`race`).
    #[tokio::test]
    async fn dead_and_slow_routes_do_not_delay_the_healthy_one() {
        let slow = fake_hub(Duration::from_secs(5), "hub");
        let lan = fake_hub(Duration::ZERO, "hub");
        let mut settings = two_link_settings(&slow.url, &lan.url);
        settings.hub_candidates.push(HubCandidate {
            url: refusing_url(),
            role: HubRole::Authoritative,
            auth: crate::hub_candidates::CandidateAuth::Global,
            sites: vec!["lab-lan".to_owned()],
            hub: Some("hub".to_owned()),
        });
        settings.server_urls = settings
            .hub_candidates
            .iter()
            .map(|candidate| candidate.url.clone())
            .collect();
        let backend = race_at(&settings, &both_links(), "net", None, false).await;
        assert_eq!(backend.url(), Some(lan.url.as_str()));
        let HubBackend::Remote { tried, probes, .. } = backend else {
            panic!("expected remote");
        };
        assert!(
            tried.contains(&slow.url) && tried.contains(&lan.url),
            "the stalled and the winning route were both started: {tried:?}"
        );
        assert_eq!(
            probes[0].error,
            Some(ProbeFailure::Cancelled),
            "the winner cancels the stalled route before its timeout"
        );
    }

    /// Protects: a fresh same-store cached alias receives the first probe.
    /// Detects: ignoring the cache or contacting the other healthy alias.
    /// Needs: private TCP listeners, a temporary cache and fixed network key.
    /// Breadcrumb: crates/agent-bus-core/src/hub_cache.rs (`HubCache::load`).
    #[tokio::test]
    async fn a_fresh_cache_hit_is_probed_first_and_wins_without_racing_the_rest() {
        // Why: after the first resolution the cached route should answer alone.
        let dir = tempfile::tempdir().expect("tempdir");
        let fabric = fake_hub(Duration::ZERO, "hub");
        let lan = fake_hub(Duration::ZERO, "hub");
        let settings = two_link_settings(&fabric.url, &lan.url);
        let locations = both_links();
        let fingerprint = routing_fingerprint(&settings.hub_candidates, Some(&locations), "net");
        cache_in(&dir)
            .store_with_rtt(
                &fingerprint,
                &lan.url,
                HubRole::Authoritative,
                Some(3),
                SystemTime::now(),
            )
            .expect("seed cache");
        let backend = race_at(&settings, &locations, "net", Some(cache_in(&dir)), false).await;
        assert_eq!(backend.url(), Some(lan.url.as_str()), "cached route first");
        assert_eq!(fabric.hits.load(Ordering::SeqCst), 0);
    }

    /// Protects: a changed attachment key invalidates prior route preference.
    /// Detects: cached LAN preference surviving a changed network fingerprint.
    /// Needs: private TCP listeners and a seeded temporary route cache.
    /// Breadcrumb: crates/agent-bus-core/src/hub_cache.rs (fingerprint validation).
    #[tokio::test]
    async fn a_changed_network_fingerprint_misses_the_cache_and_re_races() {
        // Why: plugging in a better link must not be masked by a route cached before it existed.
        let dir = tempfile::tempdir().expect("tempdir");
        let fabric = fake_hub(Duration::ZERO, "hub");
        let lan = fake_hub(Duration::ZERO, "hub");
        let settings = two_link_settings(&fabric.url, &lan.url);
        let locations = both_links();
        let fingerprint =
            routing_fingerprint(&settings.hub_candidates, Some(&locations), "lan-only");
        cache_in(&dir)
            .store_with_rtt(
                &fingerprint,
                &lan.url,
                HubRole::Authoritative,
                Some(3),
                SystemTime::now(),
            )
            .expect("seed cache");
        let same = race_at(
            &settings,
            &locations,
            "lan-only",
            Some(cache_in(&dir)),
            false,
        )
        .await;
        assert_eq!(
            same.url(),
            Some(lan.url.as_str()),
            "same network: cache hit"
        );
        let plugged = race_at(
            &settings,
            &locations,
            "lan-and-fabric",
            Some(cache_in(&dir)),
            false,
        )
        .await;
        assert_eq!(
            plugged.url(),
            Some(fabric.url.as_str()),
            "a new attachment forces a fresh race that prefers the fabric"
        );
    }

    /// Protects: a cached dead alias cannot serialize or retain same-store probes.
    /// Detects: waiting for its full timeout or failing to cancel the losing probe.
    /// Needs: paused time, private cache and an observed loser-drop notification.
    /// Breadcrumb: crates/agent-bus-core/src/link_route.rs (`race`).
    #[tokio::test(start_paused = true)]
    async fn a_failed_cached_route_is_replaced_by_a_concurrent_re_probe() {
        struct CancelOnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for CancelOnDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let dead = "http://cached.lan:8400";
        let lan = "http://alternate.lan:8400";
        let settings = two_link_settings(dead, lan);
        let locations = both_links();
        let fingerprint = routing_fingerprint(&settings.hub_candidates, Some(&locations), "net");
        cache_in(&dir)
            .store_with_rtt(
                &fingerprint,
                dead,
                HubRole::Authoritative,
                Some(1),
                SystemTime::now(),
            )
            .expect("seed cache");
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        let cancellation = Arc::new(std::sync::Mutex::new(Some(cancel_tx)));
        let started = Arc::new(AtomicBool::new(false));
        let observed_start = Arc::clone(&started);
        let probe: AsyncProbe = Arc::new(move |url: String| -> ProbeFuture {
            let is_cached = url == dead;
            assert!(is_cached || url == lan, "unexpected fixture route");
            let cancellation = Arc::clone(&cancellation);
            let started = Arc::clone(&observed_start);
            Box::pin(async move {
                if is_cached {
                    started.store(true, Ordering::SeqCst);
                    let sender = cancellation.lock().expect("cancellation").take();
                    let _guard = CancelOnDrop(sender);
                    std::future::pending().await
                } else {
                    Some(ProbeInfo {
                        build_version: None,
                        hub_identity: Some("hub".to_owned()),
                    })
                }
            })
        });
        let backend = resolve_concurrent(
            &settings,
            &probe,
            false,
            Some(&locations),
            "net",
            Some(cache_in(&dir)),
        )
        .await;
        assert_eq!(backend.url(), Some(lan));
        assert!(started.load(Ordering::SeqCst), "cached probe really began");
        let HubBackend::Remote { probes, .. } = &backend else {
            panic!("same-store alias must win");
        };
        assert_eq!(
            probes
                .iter()
                .find(|probe| probe.url == dead)
                .expect("cached route report")
                .error,
            Some(ProbeFailure::Cancelled),
            "the winner cancels the pending alias before its timeout"
        );
        tokio::time::timeout(Duration::from_secs(2), cancel_rx)
            .await
            .expect("loser cancellation must complete")
            .expect("cached pending probe was dropped");
        let remembered = cache_in(&dir)
            .load(&fingerprint, SystemTime::now())
            .expect("the new winner is cached");
        assert_eq!(remembered.url, lan);
        assert!(remembered.rtt_ms.is_some(), "the winning RTT is persisted");
    }

    /// Protects: explicit invalidation removes a just-resolved cache entry.
    /// Detects: invalidation leaving the persisted winner readable.
    /// Needs: private TCP resolution and a temporary cache; no request fault injection.
    /// Breadcrumb: crates/agent-bus-core/src/hub_cache.rs (`HubCache::invalidate`).
    #[tokio::test]
    async fn invalidating_after_a_request_failure_forces_a_full_re_probe() {
        // Why: a connection failure against the cached route must drop it immediately.
        let dir = tempfile::tempdir().expect("tempdir");
        let lan = fake_hub(Duration::ZERO, "hub");
        let settings = two_link_settings(&refusing_url(), &lan.url);
        let locations = both_links();
        let backend = race_at(&settings, &locations, "net", Some(cache_in(&dir)), false).await;
        assert_eq!(backend.url(), Some(lan.url.as_str()));
        let fingerprint = routing_fingerprint(&settings.hub_candidates, Some(&locations), "net");
        assert!(
            cache_in(&dir)
                .load(&fingerprint, SystemTime::now())
                .is_some()
        );
        cache_in(&dir).invalidate();
        assert!(
            cache_in(&dir)
                .load(&fingerprint, SystemTime::now())
                .is_none()
        );
    }

    /// Protects: claims require a genuine authority despite faster answers.
    /// Detects: accepting an identity mismatch or fallback as authority.
    /// Needs: private TCP listeners reporting genuine and foreign identities.
    /// Breadcrumb: crates/agent-bus-core/src/hub_candidates.rs (authority and identity rules).
    #[tokio::test]
    async fn claims_never_go_to_a_faster_fallback_or_a_foreign_hub() {
        // Why: a fast route must not become a claims authority unless it is the named hub.
        let impostor = fake_hub(Duration::ZERO, "someone-else");
        let genuine = fake_hub(Duration::from_millis(80), "hub");
        let mut settings = two_link_settings(&impostor.url, &genuine.url);
        settings.hub_candidates[0].role = HubRole::Authoritative;
        let backend = race_at(&settings, &both_links(), "net", None, true).await;
        let HubBackend::Remote {
            url,
            authoritative,
            probes,
            ..
        } = &backend
        else {
            panic!("expected remote, got {backend:?}");
        };
        assert_eq!(url, &genuine.url);
        assert!(authoritative);
        assert_eq!(probes[0].error, Some(ProbeFailure::IdentityMismatch));

        let mut fallback_only = two_link_settings(&impostor.url, &genuine.url);
        fallback_only.hub_candidates[1].role = HubRole::Fallback;
        let refused = race_at(&fallback_only, &both_links(), "net", None, true).await;
        assert!(refused.is_offline(), "no authoritative route proved itself");
    }

    /// Protects: a wrong site guess retains deferred authority aliases.
    /// Detects: discarding routes assigned to an unobserved location.
    /// Needs: private refused/healthy TCP routes and an unrelated location.
    /// Breadcrumb: crates/agent-bus-core/src/link_route.rs (deferred route plans).
    #[tokio::test]
    async fn a_wrong_location_guess_falls_back_to_the_deferred_routes() {
        // Why: a false-negative location must cost time, never reachability.
        let lan = fake_hub(Duration::ZERO, "hub");
        let settings = two_link_settings(&refusing_url(), &lan.url);
        let elsewhere = vec!["somewhere-else".to_owned()];
        let backend = race_at(&settings, &elsewhere, "net", None, false).await;
        assert_eq!(
            backend.url(),
            Some(lan.url.as_str()),
            "routes declared for another site are still tried when nothing else answers"
        );
    }

    /// Protects: failed configured routes yield explicit offline diagnostics.
    /// Detects: local fallback or missing per-route failure reports.
    /// Needs: two private refused loopback endpoints, no live bus.
    /// Breadcrumb: crates/agent-bus-core/src/link_route.rs (`ProbeReport`).
    #[tokio::test]
    async fn nothing_answering_is_offline_with_a_report_per_candidate() {
        // Why: offline must stay explicit and carry per-route reasons, never a local fallback.
        let settings = two_link_settings(&refusing_url(), &refusing_url());
        let backend = race_at(&settings, &both_links(), "net", None, false).await;
        let HubBackend::Offline { tried, probes } = &backend else {
            panic!("expected offline, got {backend:?}");
        };
        assert_eq!(tried.len(), 2);
        assert!(probes.iter().all(|probe| probe.error.is_some()));
        assert!(backend.to_report()["selected"].is_null());
    }

    type ProbeEvents = Arc<std::sync::Mutex<Vec<String>>>;
    type ProbeStep = (String, Duration, Option<String>);

    /// Fake only transport latency/results; exercise the actual resolver and race.
    fn scripted_probe(steps: Vec<ProbeStep>) -> (AsyncProbe, ProbeEvents) {
        let steps = Arc::new(steps);
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&events);
        let probe = Arc::new(move |url: String| -> ProbeFuture {
            let (_, delay, identity) = steps
                .iter()
                .find(|step| step.0 == url)
                .cloned()
                .expect("every contacted URL must have an explicit fixture result");
            let events = Arc::clone(&recorded);
            Box::pin(async move {
                events.lock().expect("events").push(format!("start:{url}"));
                tokio::time::sleep(delay).await;
                events.lock().expect("events").push(format!("finish:{url}"));
                identity.map(|hub| ProbeInfo {
                    build_version: None,
                    hub_identity: Some(hub),
                })
            })
        });
        (probe, events)
    }

    /// Give the cloud its own private fixture credential, never a fleet token.
    fn authority_cloud_settings() -> (Settings, tempfile::TempDir) {
        let credentials = tempfile::tempdir().expect("private fixture credentials");
        let token = credentials.path().join("cloud.token");
        std::fs::write(&token, "test-only-cloud-token").expect("fixture token");
        let mut settings =
            configured_settings(&["http://authority.lan:8400", "https://cloud.example.com"]);
        settings.hub_candidates[0].hub = Some("fleet".to_owned());
        settings.hub_candidates[0].sites = vec!["lab-fabric".to_owned()];
        settings.hub_candidates[1].role = HubRole::Cloud;
        settings.hub_candidates[1].hub = Some("cloud-store".to_owned());
        settings.hub_candidates[1].auth =
            crate::hub_candidates::CandidateAuth::TokenFile(token.to_string_lossy().into_owned());
        settings.probe_connect_timeout_ms = 400;
        settings.probe_stagger_ms = 60;
        (settings, credentials)
    }

    fn cloud_steps(settings: &Settings) -> Vec<ProbeStep> {
        vec![
            (
                settings.hub_candidates[0].url.clone(),
                Duration::from_millis(250),
                Some("fleet".to_owned()),
            ),
            (
                settings.hub_candidates[1].url.clone(),
                Duration::ZERO,
                Some("cloud-store".to_owned()),
            ),
        ]
    }

    /// Protects: reads stay on the same authority store as durable mutations.
    /// Detects: a healthy faster independent cloud winning the global race.
    /// Needs: virtual time, explicit private cloud credentials, no real network.
    /// Breadcrumb: crates/agent-bus-core/src/link_route.rs (`race`).
    #[tokio::test(start_paused = true)]
    async fn healthy_authority_precedes_a_faster_independent_cloud() {
        let (settings, _credentials) = authority_cloud_settings();
        let (probe, events) = scripted_probe(cloud_steps(&settings));
        let backend = resolve_concurrent(&settings, &probe, false, None, "net", None).await;
        assert_eq!(backend.url(), Some(settings.hub_candidates[0].url.as_str()));
        assert!(
            !events
                .lock()
                .expect("events")
                .iter()
                .any(|event| event.contains("cloud.example.com"))
        );
    }

    /// Protects: a remembered independent store cannot override authority recovery.
    /// Detects: cached-cloud promotion ahead of a reachable named authority.
    /// Needs: private cache, virtual time and explicit synthetic credentials.
    /// Breadcrumb: crates/agent-bus-core/src/hub_cache.rs (`HubCache`).
    #[tokio::test(start_paused = true)]
    async fn cached_cloud_does_not_bypass_a_healthy_authority() {
        let (settings, _credentials) = authority_cloud_settings();
        let directory = tempfile::tempdir().expect("private cache");
        let fingerprint = routing_fingerprint(&settings.hub_candidates, None, "net");
        cache_in(&directory)
            .store(
                &fingerprint,
                &settings.hub_candidates[1].url,
                HubRole::Cloud,
                SystemTime::now(),
            )
            .expect("seed cloud cache");
        let (probe, events) = scripted_probe(cloud_steps(&settings));
        let backend = resolve_concurrent(
            &settings,
            &probe,
            false,
            None,
            "net",
            Some(cache_in(&directory)),
        )
        .await;
        assert_eq!(backend.url(), Some(settings.hub_candidates[0].url.as_str()));
        assert_eq!(
            events.lock().expect("events").len(),
            2,
            "only the authority starts and finishes"
        );
    }

    /// Protects: a location guess cannot demote the real authority below cloud.
    /// Detects: globally primary cloud beating a healthy deferred authority.
    /// Needs: virtual time and an intentionally nonmatching location label.
    /// Breadcrumb: crates/agent-bus-core/src/link_route.rs (`plan_routes`).
    #[tokio::test(start_paused = true)]
    async fn deferred_authority_is_exhausted_before_independent_cloud() {
        let (settings, _credentials) = authority_cloud_settings();
        let (probe, _) = scripted_probe(cloud_steps(&settings));
        let away = vec!["elsewhere".to_owned()];
        let backend = resolve_concurrent(&settings, &probe, false, Some(&away), "net", None).await;
        assert_eq!(backend.url(), Some(settings.hub_candidates[0].url.as_str()));
    }

    /// Protects: fallback starts only after all routes to the authority fail.
    /// Detects: independent cloud starting before a deferred authority attempt.
    /// Needs: observed virtual probe start/finish order, not wall-clock timing.
    /// Breadcrumb: crates/agent-bus-core/src/link_route.rs (`race`).
    #[tokio::test(start_paused = true)]
    async fn cloud_waits_for_primary_and_deferred_authority_failures() {
        let (mut settings, _credentials) = authority_cloud_settings();
        let mut second = settings.hub_candidates[0].clone();
        second.url = "http://authority-tail.lan:8400".to_owned();
        second.sites = vec!["different-site".to_owned()];
        settings.hub_candidates.insert(1, second);
        settings.server_urls = settings
            .hub_candidates
            .iter()
            .map(|candidate| candidate.url.clone())
            .collect();
        let cloud = settings.hub_candidates[2].url.clone();
        let deferred = settings.hub_candidates[1].url.clone();
        let (probe, events) = scripted_probe(vec![
            (
                settings.hub_candidates[0].url.clone(),
                Duration::from_millis(100),
                None,
            ),
            (deferred.clone(), Duration::from_millis(100), None),
            (
                cloud.clone(),
                Duration::ZERO,
                Some("cloud-store".to_owned()),
            ),
        ]);
        let backend = resolve_concurrent(
            &settings,
            &probe,
            false,
            Some(&["lab-fabric".to_owned()]),
            "net",
            None,
        )
        .await;
        assert_eq!(backend.url(), Some(cloud.as_str()));
        let observed = events.lock().expect("events");
        let finished = observed
            .iter()
            .position(|e| e == &format!("finish:{deferred}"))
            .expect("deferred authority was exhausted");
        let started = observed
            .iter()
            .position(|e| e == &format!("start:{cloud}"))
            .expect("fallback was tried");
        assert!(
            finished < started,
            "independent fallback follows the complete authority group"
        );
    }

    /// Protects: independent fallback stores keep configured consistency order.
    /// Detects: racing unrelated stores merely because their link class matches.
    /// Needs: virtual time and explicit different logical hub identities.
    /// Breadcrumb: crates/agent-bus-core/src/hub_candidates.rs (`HubCandidate`).
    #[tokio::test(start_paused = true)]
    async fn different_fallback_stores_are_not_interchangeable_routes() {
        let mut settings =
            configured_settings(&["http://first.lan:8400", "http://second.lan:8400"]);
        for (index, candidate) in settings.hub_candidates.iter_mut().enumerate() {
            candidate.role = HubRole::Fallback;
            candidate.hub = Some(format!("store-{index}"));
        }
        let (probe, _) = scripted_probe(vec![
            (
                settings.hub_candidates[0].url.clone(),
                Duration::from_millis(250),
                Some("store-0".to_owned()),
            ),
            (
                settings.hub_candidates[1].url.clone(),
                Duration::ZERO,
                Some("store-1".to_owned()),
            ),
        ]);
        let backend = resolve_concurrent(&settings, &probe, false, None, "net", None).await;
        assert_eq!(backend.url(), Some(settings.hub_candidates[0].url.as_str()));
    }

    /// Protects: missing hub identity never implies two URLs share storage.
    /// Detects: grouping all unnamed fallbacks into one consistency domain.
    /// Needs: legacy unnamed candidates and deterministic transport latency.
    /// Breadcrumb: crates/agent-bus-core/src/hub_candidates.rs (`HubCandidate`).
    #[tokio::test(start_paused = true)]
    async fn unnamed_fallback_urls_remain_separate_stores() {
        let mut settings =
            configured_settings(&["http://first.lan:8400", "http://second.lan:8400"]);
        for candidate in &mut settings.hub_candidates {
            candidate.role = HubRole::Fallback;
        }
        let (probe, _) = scripted_probe(vec![
            (
                settings.hub_candidates[0].url.clone(),
                Duration::from_millis(250),
                Some("first".to_owned()),
            ),
            (
                settings.hub_candidates[1].url.clone(),
                Duration::ZERO,
                Some("second".to_owned()),
            ),
        ]);
        let backend = resolve_concurrent(&settings, &probe, false, None, "net", None).await;
        assert_eq!(backend.url(), Some(settings.hub_candidates[0].url.as_str()));
    }

    /// Protects: synchronous fallback resolution follows the authority barrier.
    /// Detects: serial paths retaining global cache/location-first ordering.
    /// Needs: synthetic cloud credentials, a seeded cache and observed calls.
    /// Breadcrumb: crates/agent-bus-core/src/link_route.rs (`walk`).
    #[test]
    fn serial_resolution_also_prefers_a_deferred_authority_over_cached_cloud() {
        let (settings, _credentials) = authority_cloud_settings();
        let directory = tempfile::tempdir().expect("private cache");
        let away = vec!["elsewhere".to_owned()];
        let fingerprint = routing_fingerprint(&settings.hub_candidates, Some(&away), "net");
        cache_in(&directory)
            .store(
                &fingerprint,
                &settings.hub_candidates[1].url,
                HubRole::Cloud,
                SystemTime::now(),
            )
            .expect("seed cache");
        let mut calls = Vec::new();
        let backend = resolve_serial(
            &settings,
            &mut |url: &str| {
                calls.push(url.to_owned());
                Some(ProbeInfo {
                    build_version: None,
                    hub_identity: Some(
                        if url.contains("authority") {
                            "fleet"
                        } else {
                            "cloud-store"
                        }
                        .to_owned(),
                    ),
                })
            },
            false,
            Some(&away),
            "net",
            Some(cache_in(&directory)),
        );
        assert_eq!(backend.url(), Some(settings.hub_candidates[0].url.as_str()));
        assert_eq!(calls, vec![settings.hub_candidates[0].url.clone()]);
    }
}
