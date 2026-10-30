//! Link-aware endpoint selection: classify routes, plan them, race them.
//!
//! A client that can reach the same hub over a Thunderbolt/USB4 fabric, a
//! direct Ethernet cable, the lab LAN, a tailnet and a cloud relay should use
//! the fastest of those, and should not pay a connect timeout for every dead
//! path on the way there. Walking `server_urls` one by one does exactly that:
//! four dead candidates cost four timeouts before the fifth answers.
//!
//! This module replaces the walk with a *concurrent race* in the style of
//! happy eyeballs (RFC 8305):
//!
//! 1. [`LinkClass::of`] ranks every candidate: fabric, then LAN or direct
//!    link, then tailnet, then cloud.
//! 2. [`plan_routes`] keeps the candidates that make sense at the detected
//!    network location, orders them by class, and gives each worse class a
//!    small head-start delay (the *stagger*) so a better link wins a tie.
//! 3. [`race`] starts every probe at its delay, takes the first healthy
//!    answer that the caller accepts, and drops the rest.
//!
//! Selection only changes *which route is preferred*. Roles, credentials and
//! the identity check stay with the caller (`crate::hub`), which supplies them
//! through the [`Slot::skip`] and `accept` hooks. A faster fallback or cloud
//! route can therefore never become a claims authority.
//!
//! The within-class order is deliberately not precomputed: two routes of one
//! class start together, so the one with the lower measured round-trip time is
//! the first healthy responder and wins. The winner's RTT is persisted by
//! `crate::hub_cache` and reported in `health`.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::task::JoinSet;

use crate::hub::ProbeInfo;
use crate::hub_candidates::{HubCandidate, HubRole};
use crate::network_location::Cidr;

/// Default head start, in milliseconds, between link classes. It is far above
/// the round trip of a local link and well below a probe timeout.
pub const DEFAULT_STAGGER_MS: u64 = 75;

/// The preference class of a route. Lower sorts first and wins ties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkClass {
    /// A dedicated machine-to-machine link: Thunderbolt/USB4 fabric, or a
    /// point-to-point cable (~20 Gbps, sub-millisecond).
    Fabric,
    /// A LAN, or a directly attached or forwarded route to the hub.
    Lan,
    /// A tailnet (overlay VPN) address.
    Tailnet,
    /// The off-site cloud relay. Always last.
    Cloud,
}

/// Site-name words that mark a dedicated machine-to-machine link.
const FABRIC_WORDS: &[&str] = &["fabric", "tb", "thunderbolt", "usb4", "p2p"];
/// Site-name words that mark a tailnet route.
const TAILNET_WORDS: &[&str] = &["tailnet", "tailscale"];

impl LinkClass {
    /// Lowercase name used in `health` output.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fabric => "fabric",
            Self::Lan => "lan",
            Self::Tailnet => "tailnet",
            Self::Cloud => "cloud",
        }
    }

    /// Classify `candidate` from its role, declared `sites` and address.
    ///
    /// A `cloud` role is [`Self::Cloud`] whatever its sites say. Otherwise a
    /// site named like a fabric (`lab-fabric`, `usb4`, `tb-link`, `p2p`)
    /// makes it [`Self::Fabric`], a `tailnet` site or a `*.ts.net` or
    /// `100.64.0.0/10` address makes it [`Self::Tailnet`], and anything else is
    /// [`Self::Lan`]. Site names are split on non-alphanumeric characters, so
    /// `tb` matches `tb-link` but not `outbound`.
    #[must_use]
    pub fn of(candidate: &HubCandidate) -> Self {
        if candidate.role == HubRole::Cloud {
            return Self::Cloud;
        }
        let has_word = |words: &[&str]| {
            candidate.sites.iter().any(|site| {
                site.split(|ch: char| !ch.is_ascii_alphanumeric())
                    .any(|word| words.iter().any(|wanted| word.eq_ignore_ascii_case(wanted)))
            })
        };
        if has_word(FABRIC_WORDS) {
            Self::Fabric
        } else if has_word(TAILNET_WORDS) || tailnet_address(&candidate.url) {
            Self::Tailnet
        } else {
            Self::Lan
        }
    }
}

fn tailnet_address(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.host() {
        Some(url::Host::Domain(name)) => name.to_ascii_lowercase().ends_with(".ts.net"),
        Some(url::Host::Ipv4(address)) => Cidr::parse("100.64.0.0/10")
            .is_ok_and(|cgnat| cgnat.contains(std::net::IpAddr::V4(address))),
        _ => false,
    }
}

/// One route in a [`RoutePlan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRoute {
    /// Index into the candidate list the plan was built from.
    pub index: usize,
    /// The route's preference class.
    pub class: LinkClass,
    /// How long after the race starts this route's probe begins.
    pub delay: Duration,
    /// `true` when this is the last-good route from the cache.
    pub cached: bool,
}

/// The routes to try, in order, split into two stages.
///
/// `primary` is raced first. `deferred` holds the candidates that were
/// declared for some *other* location; they are only raced when every primary
/// route failed, so a wrong location guess costs time but never reachability.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoutePlan {
    /// Routes that fit the detected location, best class first.
    pub primary: Vec<PlannedRoute>,
    /// Routes declared for another location, tried only after `primary` fails.
    pub deferred: Vec<PlannedRoute>,
}

/// Inputs to [`plan_routes`].
#[derive(Debug, Clone, Copy)]
pub struct PlanRequest<'a> {
    /// The resolved candidates, in configured order.
    pub candidates: &'a [HubCandidate],
    /// The detected locations, or `None` when detection is off.
    pub locations: Option<&'a [String]>,
    /// The last-good candidate from the route cache, if still fresh.
    pub cached: Option<usize>,
    /// Keep only [`HubRole::Authoritative`] candidates (claims).
    pub authority_only: bool,
    /// Head start between consecutive waves of the plan.
    pub stagger: Duration,
}

/// `true` when `candidate` belongs in the first stage at `locations`.
///
/// Cloud, tailnet and site-less routes work from anywhere. A route declared
/// for specific sites is eligible when one of them is a detected location.
/// When detection is off or matched nothing, every route is eligible: an
/// unknown network must not hide a reachable authority.
fn eligible(candidate: &HubCandidate, class: LinkClass, locations: Option<&[String]>) -> bool {
    let Some(locations) = locations.filter(|names| !names.is_empty()) else {
        return true;
    };
    matches!(class, LinkClass::Cloud | LinkClass::Tailnet)
        || candidate.sites.is_empty()
        || site_matches(candidate, locations)
}

fn site_matches(candidate: &HubCandidate, locations: &[String]) -> bool {
    candidate
        .sites
        .iter()
        .any(|site| locations.iter().any(|location| location == site))
}

/// A candidate awaiting placement in a stage.
struct Entry {
    index: usize,
    class: LinkClass,
    /// Authoritative routes lead fallbacks of the same link class.
    role_rank: u8,
    cached: bool,
    site_match: bool,
}

/// Order and stagger one stage. The cached route leads at no delay; every
/// following change of (link class, role) starts a new wave one stagger later,
/// so a configured authority beats a fallback on the same kind of link, while
/// routes of equal class and role start together and the faster one wins.
fn arrange(mut routes: Vec<Entry>, stagger: Duration) -> Vec<PlannedRoute> {
    routes.sort_by_key(|entry| {
        (
            !entry.cached,
            entry.class,
            entry.role_rank,
            !entry.site_match,
            entry.index,
        )
    });
    let mut wave: u32 = 0;
    let mut previous: Option<(bool, LinkClass, u8)> = None;
    routes
        .into_iter()
        .map(|entry| {
            let key = (entry.cached, entry.class, entry.role_rank);
            if previous.is_some_and(|last| last != key) {
                wave += 1;
            }
            previous = Some(key);
            PlannedRoute {
                index: entry.index,
                class: entry.class,
                delay: stagger.saturating_mul(wave),
                cached: entry.cached,
            }
        })
        .collect()
}

/// Build the [`RoutePlan`] for `request`.
#[must_use]
pub fn plan_routes(request: &PlanRequest<'_>) -> RoutePlan {
    let mut primary = Vec::new();
    let mut deferred = Vec::new();
    for (index, candidate) in request.candidates.iter().enumerate() {
        if request.authority_only && candidate.role != HubRole::Authoritative {
            continue;
        }
        let class = LinkClass::of(candidate);
        let matched = request
            .locations
            .is_some_and(|locations| site_matches(candidate, locations));
        let stage = if eligible(candidate, class, request.locations) {
            &mut primary
        } else {
            &mut deferred
        };
        stage.push(Entry {
            index,
            class,
            role_rank: match candidate.role {
                HubRole::Authoritative => 0,
                HubRole::Fallback => 1,
                HubRole::Cloud => 2,
            },
            cached: request.cached == Some(index),
            site_match: matched,
        });
    }
    RoutePlan {
        primary: arrange(primary, request.stagger),
        deferred: arrange(deferred, request.stagger),
    }
}

/// Why a route did not win.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeFailure {
    /// No usable credential for the route; it was not contacted.
    CredentialUnavailable,
    /// Unreachable, refused, non-2xx or a malformed `/health` body.
    Unhealthy,
    /// No answer within the per-route budget.
    TimedOut,
    /// It answered, but as a different hub than the candidate names.
    IdentityMismatch,
    /// A faster route won while this probe was in flight.
    Cancelled,
    /// A faster route won before this probe's turn came.
    Skipped,
}

impl ProbeFailure {
    /// `true` when the route was contacted (or would have been, but for a
    /// local credential problem), so it belongs in `tried`.
    #[must_use]
    pub const fn was_attempted(self) -> bool {
        !matches!(self, Self::Skipped)
    }
}

/// What happened to one route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotOutcome {
    /// A healthy, accepted answer after `rtt`.
    Healthy {
        /// Probe round trip.
        rtt: Duration,
    },
    /// The route did not win.
    Failed(ProbeFailure),
}

/// One probe to run, produced by the caller from a [`PlannedRoute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    /// The hub base URL to probe.
    pub url: String,
    /// Delay before the probe starts.
    pub delay: Duration,
    /// Set when the route must not be contacted (for example no credential).
    pub skip: Option<ProbeFailure>,
}

/// The winning route of a [`race`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Winner {
    /// Index into the `slots` passed to [`race`].
    pub slot: usize,
    /// What the winner's `/health` reported.
    pub info: ProbeInfo,
    /// Probe round trip.
    pub rtt: Duration,
}

/// The result of one [`race`] (or one serial walk): an outcome per slot and
/// the winner, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RaceResult {
    /// One entry per slot, in slot order.
    pub outcomes: Vec<SlotOutcome>,
    /// The accepted healthy route, if any.
    pub winner: Option<Winner>,
}

/// A boxed probe future.
pub type ProbeFuture = Pin<Box<dyn Future<Output = Option<ProbeInfo>> + Send>>;

/// An asynchronous `/health` probe for one hub base URL, shareable across the
/// tasks of a race. `None` means unreachable or unhealthy for any reason.
pub type AsyncProbe = Arc<dyn Fn(String) -> ProbeFuture + Send + Sync>;

/// Race `slots`: start each probe after its delay, return the first healthy
/// answer that `accept` approves, and drop every other probe.
///
/// `budget` bounds each probe individually, including name resolution, so one
/// dead name cannot hold up the race. `accept` runs once per healthy answer
/// and carries the caller's checks (hub identity, authority); a healthy but
/// rejected answer is recorded as [`ProbeFailure::IdentityMismatch`] and the
/// race goes on. Dropping the losers cancels in-flight probes, which closes
/// their sockets.
pub async fn race(
    slots: &[Slot],
    probe: &AsyncProbe,
    budget: Duration,
    mut accept: impl FnMut(usize, &ProbeInfo) -> bool,
) -> RaceResult {
    enum Probed {
        Healthy(ProbeInfo),
        Unhealthy,
        TimedOut,
    }

    let started: Arc<Vec<AtomicBool>> =
        Arc::new(slots.iter().map(|_slot| AtomicBool::new(false)).collect());
    let mut outcomes: Vec<Option<SlotOutcome>> = vec![None; slots.len()];
    let mut set: JoinSet<(usize, Duration, Probed)> = JoinSet::new();
    for (index, slot) in slots.iter().enumerate() {
        if let Some(reason) = slot.skip {
            outcomes[index] = Some(SlotOutcome::Failed(reason));
            continue;
        }
        let probe = Arc::clone(probe);
        let started = Arc::clone(&started);
        let (url, delay) = (slot.url.clone(), slot.delay);
        set.spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            started[index].store(true, Ordering::Relaxed);
            let begun = Instant::now();
            let probed = match tokio::time::timeout(budget, probe(url)).await {
                Ok(Some(info)) => Probed::Healthy(info),
                Ok(None) => Probed::Unhealthy,
                Err(_elapsed) => Probed::TimedOut,
            };
            (index, begun.elapsed(), probed)
        });
    }

    let mut winner = None;
    while let Some(joined) = set.join_next().await {
        let Ok((index, rtt, probed)) = joined else {
            continue;
        };
        outcomes[index] = Some(match probed {
            Probed::Healthy(info) if accept(index, &info) => {
                winner = Some(Winner {
                    slot: index,
                    info,
                    rtt,
                });
                SlotOutcome::Healthy { rtt }
            }
            Probed::Healthy(_rejected) => SlotOutcome::Failed(ProbeFailure::IdentityMismatch),
            Probed::Unhealthy => SlotOutcome::Failed(ProbeFailure::Unhealthy),
            Probed::TimedOut => SlotOutcome::Failed(ProbeFailure::TimedOut),
        });
        if winner.is_some() {
            break;
        }
    }
    // Dropping the set aborts every probe still running.
    drop(set);

    let outcomes = outcomes
        .into_iter()
        .enumerate()
        .map(|(index, outcome)| {
            outcome.unwrap_or_else(|| {
                SlotOutcome::Failed(if !started[index].load(Ordering::Relaxed) {
                    ProbeFailure::Skipped
                } else if winner.is_some() {
                    ProbeFailure::Cancelled
                } else {
                    // A probe task that panicked: report it as a failed probe.
                    ProbeFailure::Unhealthy
                })
            })
        })
        .collect();
    RaceResult { outcomes, winner }
}

/// Walk `slots` one at a time, in order, with a synchronous `probe`.
///
/// The deterministic counterpart of [`race`] for callers that inject a plain
/// closure (unit tests, simple transports). It ignores each slot's `delay`,
/// stops at the first accepted answer, and marks later slots
/// [`ProbeFailure::Skipped`].
pub fn walk(
    slots: &[Slot],
    probe: &mut impl FnMut(&str) -> Option<ProbeInfo>,
    mut accept: impl FnMut(usize, &ProbeInfo) -> bool,
) -> RaceResult {
    let mut outcomes = Vec::with_capacity(slots.len());
    let mut winner = None;
    for (index, slot) in slots.iter().enumerate() {
        if winner.is_some() {
            outcomes.push(SlotOutcome::Failed(ProbeFailure::Skipped));
            continue;
        }
        if let Some(reason) = slot.skip {
            outcomes.push(SlotOutcome::Failed(reason));
            continue;
        }
        let begun = Instant::now();
        outcomes.push(match probe(&slot.url) {
            Some(info) if accept(index, &info) => {
                let rtt = begun.elapsed();
                winner = Some(Winner {
                    slot: index,
                    info,
                    rtt,
                });
                SlotOutcome::Healthy { rtt }
            }
            Some(_rejected) => SlotOutcome::Failed(ProbeFailure::IdentityMismatch),
            None => SlotOutcome::Failed(ProbeFailure::Unhealthy),
        });
    }
    RaceResult { outcomes, winner }
}

/// One candidate's line in the `health` report. Never carries a credential,
/// a header or a response body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProbeReport {
    /// The candidate URL, with any userinfo redacted.
    pub url: String,
    /// The route's preference class.
    pub class: LinkClass,
    /// Round trip of a healthy answer, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u64>,
    /// Why the route did not win.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ProbeFailure>,
}

impl ProbeReport {
    /// Report `outcome` for the route at `url`.
    #[must_use]
    pub fn new(url: &str, class: LinkClass, outcome: SlotOutcome) -> Self {
        let (rtt_ms, error) = match outcome {
            SlotOutcome::Healthy { rtt } => (
                Some(u64::try_from(rtt.as_millis()).unwrap_or(u64::MAX)),
                None,
            ),
            SlotOutcome::Failed(reason) => (None, Some(reason)),
        };
        Self {
            url: crate::settings::redact_url(url),
            class,
            rtt_ms,
            error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub_candidates::CandidateAuth;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::sync::atomic::AtomicUsize;

    fn candidate(url: &str, role: HubRole, sites: &[&str]) -> HubCandidate {
        HubCandidate {
            url: url.to_owned(),
            role,
            auth: CandidateAuth::Global,
            sites: sites.iter().map(|site| (*site).to_owned()).collect(),
            hub: None,
        }
    }

    /// The deployed fleet shape: two LAN names, an SSH-forwarded loopback, a
    /// fabric name, a tailnet name and the cloud relay.
    fn fleet() -> Vec<HubCandidate> {
        vec![
            candidate("http://hub.lan:8400", HubRole::Authoritative, &["lab-lan"]),
            candidate(
                "http://hub.local:8400",
                HubRole::Authoritative,
                &["lab-lan"],
            ),
            candidate(
                "http://127.0.0.1:18480",
                HubRole::Authoritative,
                &["umich", "home-lan"],
            ),
            candidate(
                "http://hub-p2p:8400",
                HubRole::Authoritative,
                &["lab-fabric"],
            ),
            candidate("http://hub:8400", HubRole::Authoritative, &["tailnet"]),
            candidate("https://relay.example.com", HubRole::Cloud, &[]),
        ]
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    fn request<'a>(
        candidates: &'a [HubCandidate],
        locations: Option<&'a [String]>,
    ) -> PlanRequest<'a> {
        PlanRequest {
            candidates,
            locations,
            cached: None,
            authority_only: false,
            stagger: Duration::from_millis(75),
        }
    }

    fn indexes(stage: &[PlannedRoute]) -> Vec<usize> {
        stage.iter().map(|route| route.index).collect()
    }

    /// Protects: fabric, LAN, tailnet and cloud classification boundaries.
    /// Detects: substring fabric matches, missed tailnets or role override loss.
    /// Needs: synthetic candidates, site words and tailnet address examples.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[test]
    fn classes_follow_site_names_roles_and_tailnet_addresses() {
        // Why: the preference order fabric > lan > tailnet > cloud depends on this mapping.
        let fleet = fleet();
        let classes: Vec<LinkClass> = fleet.iter().map(LinkClass::of).collect();
        assert_eq!(
            classes,
            vec![
                LinkClass::Lan,
                LinkClass::Lan,
                LinkClass::Lan,
                LinkClass::Fabric,
                LinkClass::Tailnet,
                LinkClass::Cloud
            ]
        );
        for (site, class) in [
            ("usb4-direct", LinkClass::Fabric),
            ("TB_link", LinkClass::Fabric),
            ("thunderbolt", LinkClass::Fabric),
            // "tb" must match as a whole word, not inside "outbound".
            ("outbound", LinkClass::Lan),
            ("direct-ethernet", LinkClass::Lan),
        ] {
            let route = candidate("http://h:1", HubRole::Fallback, &[site]);
            assert_eq!(LinkClass::of(&route), class, "{site}");
        }
        for url in ["http://100.64.0.3:8400", "http://host.tail1234.ts.net:8400"] {
            let route = candidate(url, HubRole::Fallback, &[]);
            assert_eq!(LinkClass::of(&route), LinkClass::Tailnet, "{url}");
        }
        let cloud = candidate("http://h:1", HubRole::Cloud, &["lab-fabric"]);
        assert_eq!(LinkClass::of(&cloud), LinkClass::Cloud, "role beats site");
    }

    /// Protects: eligible route class ordering and deferred route retention.
    /// Detects: discarding wrong-site aliases or mixing deferred/primary plans.
    /// Needs: synthetic multi-link candidates and an explicit location.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[test]
    fn plan_keeps_site_matches_and_always_eligible_routes_best_class_first() {
        // Why: on the lab LAN only LAN routes, the tailnet and the cloud are worth racing.
        let fleet = fleet();
        let lab = names(&["lab-lan"]);
        let plan = plan_routes(&request(&fleet, Some(&lab)));
        assert_eq!(indexes(&plan.primary), vec![0, 1, 4, 5]);
        assert_eq!(
            indexes(&plan.deferred),
            vec![3, 2],
            "other locations' routes wait, best class first"
        );
        let classes: Vec<LinkClass> = plan.primary.iter().map(|route| route.class).collect();
        assert_eq!(
            classes,
            vec![
                LinkClass::Lan,
                LinkClass::Lan,
                LinkClass::Tailnet,
                LinkClass::Cloud
            ]
        );
    }

    /// Protects: a declared fabric location promotes its fabric alias.
    /// Detects: selecting LAN before fabric or losing deferred routes.
    /// Needs: synthetic LAN/fabric locations and candidate indexes.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[test]
    fn a_fabric_location_puts_the_fabric_route_first() {
        // Why: plugging in the Thunderbolt link must make it the preferred path.
        let fleet = fleet();
        let both = names(&["lab-lan", "lab-fabric"]);
        let plan = plan_routes(&request(&fleet, Some(&both)));
        assert_eq!(indexes(&plan.primary), vec![3, 0, 1, 4, 5]);
        assert_eq!(plan.primary[0].class, LinkClass::Fabric);
        assert!(plan.deferred.iter().all(|route| route.index == 2));
    }

    /// Protects: unknown detection never permanently hides declared routes.
    /// Detects: omitting candidates from primary plus deferred plans.
    /// Needs: synthetic absent, empty and unrelated location sets.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[test]
    fn unknown_or_undetected_location_includes_every_route() {
        // Why: a wrong guess must never hide a reachable authority.
        let fleet = fleet();
        for locations in [None, Some(Vec::new())] {
            let plan = plan_routes(&request(&fleet, locations.as_deref()));
            assert_eq!(plan.primary.len(), fleet.len());
            assert_eq!(plan.deferred, Vec::new());
        }
        let elsewhere = names(&["somewhere-else"]);
        let plan = plan_routes(&request(&fleet, Some(&elsewhere)));
        assert_eq!(
            indexes(&plan.primary),
            vec![4, 5],
            "a known but unrelated location keeps only the always-eligible routes"
        );
        assert_eq!(indexes(&plan.deferred), vec![3, 0, 1, 2]);
    }

    /// Protects: class-level stagger arithmetic and zero-stagger behavior.
    /// Detects: wrong delay groups or delay when staggering is disabled.
    /// Needs: pure route plans with fixed fixture stagger values.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[test]
    fn staggering_gives_each_worse_class_one_more_head_start() {
        // Why: a better link should win a tie, so a worse one starts later.
        let fleet = fleet();
        let lab = names(&["lab-lan", "lab-fabric"]);
        let plan = plan_routes(&request(&fleet, Some(&lab)));
        let delays: Vec<u128> = plan
            .primary
            .iter()
            .map(|route| route.delay.as_millis())
            .collect();
        // fabric | lan lan | tailnet | cloud
        assert_eq!(delays, vec![0, 75, 75, 150, 225]);
        let mut instant = request(&fleet, Some(&lab));
        instant.stagger = Duration::ZERO;
        assert!(
            plan_routes(&instant)
                .primary
                .iter()
                .all(|route| route.delay.is_zero()),
            "a zero stagger probes everything at once"
        );
    }

    /// Protects: cached-first ordering within the supplied route group.
    /// Detects: delayed cached probe or missing alternative head start.
    /// Needs: synthetic cached candidate index and pure route plan.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[test]
    fn the_cached_route_leads_with_no_delay_and_others_follow() {
        // Why: a fresh last-good route is verified first, ahead of its own class.
        let fleet = fleet();
        let lab = names(&["lab-lan"]);
        let mut cached = request(&fleet, Some(&lab));
        cached.cached = Some(4);
        let plan = plan_routes(&cached);
        assert_eq!(indexes(&plan.primary), vec![4, 0, 1, 5]);
        assert!(plan.primary[0].cached && plan.primary[0].delay.is_zero());
        assert_eq!(plan.primary[1].delay, Duration::from_millis(75));
    }

    /// Protects: authority-only plans exclude cloud and fallback roles.
    /// Detects: a non-authority candidate included in claims plans.
    /// Needs: synthetic role-labelled candidates with `authority_only` enabled.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[test]
    fn claims_race_only_authoritative_routes() {
        // Why: the cloud tier must never be probed as a claims authority.
        let fleet = fleet();
        let mut claims = request(&fleet, None);
        claims.authority_only = true;
        let plan = plan_routes(&claims);
        assert_eq!(indexes(&plan.primary), vec![3, 0, 1, 2, 4]);
        assert!(plan.primary.iter().all(|route| route.index != 5));
    }

    /// Protects: role preference for otherwise equal link classes.
    /// Detects: fallback preceding authority or missing configured stagger.
    /// Needs: synthetic equal-class candidates with opposite roles.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[test]
    fn an_authority_leads_a_fallback_on_the_same_kind_of_link() {
        // Why: configured priority must survive when two routes are the same link class.
        let routes = vec![
            candidate("http://fallback.lan:1", HubRole::Fallback, &[]),
            candidate("http://authority.lan:1", HubRole::Authoritative, &[]),
        ];
        let plan = plan_routes(&request(&routes, None));
        assert_eq!(indexes(&plan.primary), vec![1, 0]);
        assert_eq!(plan.primary[1].delay, Duration::from_millis(75));
    }

    // ---- race tests against real local sockets -------------------------

    /// What a fake hub does when probed.
    #[derive(Clone, Copy)]
    enum Behavior {
        /// Answers `/health` immediately.
        Healthy,
        /// Accepts, then answers only after the given delay.
        Slow(Duration),
    }

    struct FakeHub {
        url: String,
        hits: Arc<AtomicUsize>,
        stop: Arc<AtomicBool>,
        address: SocketAddr,
    }

    impl Drop for FakeHub {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            // Wake the blocking accept so the thread can exit.
            let _wake = std::net::TcpStream::connect(self.address);
        }
    }

    fn fake_hub(behavior: Behavior, identity: &'static str) -> FakeHub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake hub");
        let address = listener.local_addr().expect("local address");
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
                    if let Behavior::Slow(delay) = behavior {
                        std::thread::sleep(delay);
                    }
                    let body = format!(r#"{{"ok":true,"hub_identity":"{identity}"}}"#);
                    let _write = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
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

    /// A URL on which nothing listens: bind an ephemeral port, then close it.
    fn refusing_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let address = listener.local_addr().expect("address");
        drop(listener);
        format!("http://{address}")
    }

    /// A real, minimal `/health` probe over a TCP socket.
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
                if !response.starts_with("HTTP/1.1 200") {
                    return None;
                }
                let body = response.split("\r\n\r\n").nth(1)?;
                let value: serde_json::Value = serde_json::from_str(body).ok()?;
                Some(ProbeInfo {
                    build_version: None,
                    hub_identity: value
                        .get("hub_identity")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                })
            })
        })
    }

    /// A socket-free probe answering each URL with a fixed identity after a
    /// latency (milliseconds) that is slept on the tokio clock, so a paused
    /// clock makes the race order independent of host load.
    fn scripted_probe(answers: &[(&str, u64, Option<&str>)]) -> AsyncProbe {
        let answers: Arc<Vec<(String, u64, Option<String>)>> = Arc::new(
            answers
                .iter()
                .map(|(url, ms, hub)| ((*url).to_owned(), *ms, hub.map(str::to_owned)))
                .collect(),
        );
        Arc::new(move |url: String| -> ProbeFuture {
            let answer = answers
                .iter()
                .find(|answer| answer.0 == url)
                .cloned()
                .expect("every contacted URL must have an explicit fixture answer");
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(answer.1)).await;
                answer.2.map(|hub| ProbeInfo {
                    build_version: None,
                    hub_identity: Some(hub),
                })
            })
        })
    }

    fn slot(url: &str, delay_ms: u64) -> Slot {
        Slot {
            url: url.to_owned(),
            delay: Duration::from_millis(delay_ms),
            skip: None,
        }
    }

    const BUDGET: Duration = Duration::from_millis(400);

    /// Protects: a healthy socket wins and cancels the stalled loser.
    /// Detects: waiting for the losing timeout or retaining its probe.
    /// Needs: private slow/healthy TCP listeners under the fixture budget.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[tokio::test]
    async fn the_first_healthy_answer_wins_and_the_slow_route_is_cancelled() {
        // Why: a stalled path must not delay a healthy one, and the loser must be dropped.
        let slow = fake_hub(Behavior::Slow(Duration::from_secs(5)), "hub");
        let healthy = fake_hub(Behavior::Healthy, "hub");
        let slots = [slot(&slow.url, 0), slot(&healthy.url, 0)];
        let begun = Instant::now();
        let result = race(&slots, &socket_probe(), BUDGET, |_, _| true).await;
        assert_eq!(result.winner.as_ref().map(|winner| winner.slot), Some(1));
        assert_eq!(
            result.outcomes[0],
            SlotOutcome::Failed(ProbeFailure::Cancelled)
        );
        assert!(
            begun.elapsed() < BUDGET,
            "the race must end at the winner, not at the loser's timeout"
        );
    }

    /// Protects: a refused socket cannot prevent another route winning.
    /// Detects: offline result or an invalid refused-route outcome.
    /// Needs: a private refused loopback port and healthy TCP listener.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[tokio::test]
    async fn a_dead_route_does_not_block_the_winner() {
        // Why: a route with nothing listening must never hold up a healthy one.
        let healthy = fake_hub(Behavior::Healthy, "hub");
        let slots = [slot(&refusing_url(), 0), slot(&healthy.url, 0)];
        let result = race(&slots, &socket_probe(), BUDGET, |_, _| true).await;
        assert_eq!(result.winner.as_ref().map(|winner| winner.slot), Some(1));
        // Windows reports a refused loopback connect later than a healthy
        // answer, so the dead route is either failed or still in flight.
        assert!(matches!(
            result.outcomes[0],
            SlotOutcome::Failed(ProbeFailure::Unhealthy | ProbeFailure::Cancelled)
        ));
    }

    /// Protects: head-start preference avoids an unnecessary worse-route probe.
    /// Detects: wrong winner, missing skipped outcome or worse-route probe.
    /// Needs: virtual time, a scripted probe and distinct fixture start delays.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[tokio::test(start_paused = true)]
    async fn a_better_class_wins_a_tie_because_the_worse_class_starts_later() {
        // Why: with two healthy routes the preferred link class must be chosen. Virtual
        // time keeps a loaded host from outlasting the worse class's 75 ms head start.
        let probe = scripted_probe(&[
            ("http://fabric.test", 0, Some("hub")),
            ("http://lan.test", 0, Some("hub")),
        ]);
        let slots = [slot("http://fabric.test", 0), slot("http://lan.test", 75)];
        let result = race(&slots, &probe, BUDGET, |_, _| true).await;
        assert_eq!(result.winner.as_ref().map(|winner| winner.slot), Some(0));
        assert_eq!(
            result.outcomes[1],
            SlotOutcome::Failed(ProbeFailure::Skipped),
            "the worse class never even started"
        );
    }

    /// Protects: stagger preference does not lock in a slower link.
    /// Detects: a slow preferred route winning over a faster alternate.
    /// Needs: virtual time, a scripted probe and explicit fixture latencies.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[tokio::test(start_paused = true)]
    async fn a_much_slower_better_class_loses_to_a_healthy_worse_one() {
        // Why: the stagger is a tie-breaker, not a lock-in to a link that is slow right now.
        let probe = scripted_probe(&[
            ("http://fabric.test", 250, Some("hub")),
            ("http://lan.test", 0, Some("hub")),
        ]);
        let slots = [slot("http://fabric.test", 0), slot("http://lan.test", 50)];
        let result = race(&slots, &probe, BUDGET, |_, _| true).await;
        assert_eq!(result.winner.as_ref().map(|winner| winner.slot), Some(1));
    }

    /// Protects: a stalled route is bounded by the probe budget.
    /// Detects: missing timeout outcome, false winner or excessive wait.
    /// Needs: a private stalled TCP listener and short fixture budget.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[tokio::test]
    async fn a_route_that_exceeds_the_budget_is_timed_out_and_not_waited_for() {
        // Why: the per-route budget is what bounds a hung (e.g. unresolvable) endpoint.
        let hung = fake_hub(Behavior::Slow(Duration::from_secs(5)), "hub");
        let slots = [slot(&hung.url, 0)];
        let begun = Instant::now();
        let result = race(
            &slots,
            &socket_probe(),
            Duration::from_millis(120),
            |_, _| true,
        )
        .await;
        assert!(result.winner.is_none());
        assert_eq!(
            result.outcomes[0],
            SlotOutcome::Failed(ProbeFailure::TimedOut)
        );
        assert!(begun.elapsed() < Duration::from_secs(2));
    }

    /// Protects: the caller can reject a healthy but foreign identity.
    /// Detects: identity rejection winning or losing its failure diagnostic.
    /// Needs: virtual time, a scripted probe and an accept callback.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[tokio::test(start_paused = true)]
    async fn a_rejected_answer_is_not_a_win_and_the_next_route_is_used() {
        // Why: the caller's identity check must be able to veto a fast but wrong hub.
        // Virtual time guarantees the impostor answers before the genuine route's 40 ms
        // head start expires; real sockets let a loaded host reorder the two.
        let probe = scripted_probe(&[
            ("http://impostor.test", 0, Some("someone-else")),
            ("http://genuine.test", 0, Some("hub")),
        ]);
        let slots = [
            slot("http://impostor.test", 0),
            slot("http://genuine.test", 40),
        ];
        let result = race(&slots, &probe, BUDGET, |_, info| {
            info.hub_identity.as_deref() == Some("hub")
        })
        .await;
        assert_eq!(result.winner.as_ref().map(|winner| winner.slot), Some(1));
        assert_eq!(
            result.outcomes[0],
            SlotOutcome::Failed(ProbeFailure::IdentityMismatch)
        );
    }

    /// Protects: credential-unavailable routes receive no socket probe.
    /// Detects: network contact, false winner or lost skip diagnostic.
    /// Needs: a private counting listener and credential skip reason.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[tokio::test]
    async fn a_skipped_route_is_never_contacted() {
        // Why: a route without a usable credential must not receive a probe at all.
        let guarded = fake_hub(Behavior::Healthy, "hub");
        let mut blocked = slot(&guarded.url, 0);
        blocked.skip = Some(ProbeFailure::CredentialUnavailable);
        let result = race(&[blocked], &socket_probe(), BUDGET, |_, _| true).await;
        assert!(result.winner.is_none());
        assert_eq!(
            result.outcomes[0],
            SlotOutcome::Failed(ProbeFailure::CredentialUnavailable)
        );
        assert_eq!(guarded.hits.load(Ordering::SeqCst), 0);
    }

    /// Protects: serial resolution stops at its first accepted answer.
    /// Detects: probing beyond the winner or selecting the wrong slot.
    /// Needs: a deterministic injected probe and observed call list.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[test]
    fn walk_stops_at_the_first_accepted_answer_in_order() {
        // Why: the deterministic walk must keep the historical first-success-in-order behavior.
        let slots = [
            slot("http://a", 0),
            slot("http://b", 0),
            slot("http://c", 0),
        ];
        let mut probed = Vec::new();
        let result = walk(
            &slots,
            &mut |url: &str| {
                probed.push(url.to_owned());
                (url != "http://a").then(ProbeInfo::default)
            },
            |_, _| true,
        );
        assert_eq!(probed, vec!["http://a", "http://b"]);
        assert_eq!(result.winner.map(|winner| winner.slot), Some(1));
        assert_eq!(
            result.outcomes[2],
            SlotOutcome::Failed(ProbeFailure::Skipped)
        );
    }

    /// Protects: probe JSON preserves class/RTT/error while redacting secrets.
    /// Detects: credential leakage or incompatible probe report fields.
    /// Needs: in-memory outcomes and a synthetic URL containing userinfo.
    /// Breadcrumb: crates/agent-bus-core/src/hub.rs (route planner and race consumers).
    #[test]
    fn reports_carry_class_and_rtt_or_error_and_redact_userinfo() {
        // Why: `health` shows {url, class, rtt_ms|error} and must never echo credentials.
        let healthy = ProbeReport::new(
            "http://hub:8400",
            LinkClass::Fabric,
            SlotOutcome::Healthy {
                rtt: Duration::from_micros(1_900),
            },
        );
        let value = serde_json::to_value(&healthy).expect("serializable");
        assert_eq!(
            value,
            serde_json::json!({"url": "http://hub:8400", "class": "fabric", "rtt_ms": 1})
        );
        let failed = ProbeReport::new(
            "http://user:secret@hub:8400",
            LinkClass::Cloud,
            SlotOutcome::Failed(ProbeFailure::TimedOut),
        );
        let value = serde_json::to_value(&failed).expect("serializable");
        assert_eq!(value["error"], "timed_out");
        assert!(value.get("rtt_ms").is_none());
        assert!(!value.to_string().contains("secret"));
    }
}
