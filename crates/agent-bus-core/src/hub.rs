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
//! This module does no I/O of its own: callers supply a `probe` closure so
//! the actual transport (blocking `reqwest` today) stays out of
//! `agent-bus-core`, keeping this crate's dependency footprint unchanged and
//! making [`resolve_hub`]'s selection logic trivially unit-testable with a
//! fake probe.

use serde::Serialize;

/// What a candidate hub's `/health` probe found, when it succeeded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeInfo {
    /// The hub's `build_version` (see [`crate::build_info::BUILD_VERSION`]),
    /// when the probe response carried one.
    pub build_version: Option<String>,
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
        /// Every candidate URL tried, in order, including the one that
        /// answered (last element).
        tried: Vec<String>,
    },
    /// One or more candidates were configured but none answered within the
    /// connect timeout. Reads must be labelled `offline`/`stale`, never
    /// treated as authoritative; writes (send, presence, claim, ack) must be
    /// refused loudly rather than silently applied to a local store.
    Offline {
        /// Every candidate URL tried, in order.
        tried: Vec<String>,
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
            };
        }
    }
    HubBackend::Offline { tried }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn first_candidate_answering_is_authoritative() {
        let candidates = urls(&["http://a:8400", "http://b:8400"]);
        let backend = resolve_hub(&candidates, |url| {
            assert_eq!(url, "http://a:8400", "must try candidates in order");
            Some(ProbeInfo {
                build_version: Some("0.5.0 (abc123)".to_owned()),
            })
        });
        assert_eq!(
            backend,
            HubBackend::Remote {
                url: "http://a:8400".to_owned(),
                authoritative: true,
                hub_build: Some("0.5.0 (abc123)".to_owned()),
                tried: urls(&["http://a:8400"]),
            }
        );
        assert!(backend.is_remote());
        assert_eq!(backend.url(), Some("http://a:8400"));
    }

    #[test]
    fn second_candidate_answering_is_a_fallback_not_authoritative() {
        let candidates = urls(&["http://dead:8400", "http://b:8400", "http://c:8400"]);
        let backend = resolve_hub(&candidates, |url| {
            (url == "http://b:8400").then_some(ProbeInfo {
                build_version: None,
            })
        });
        assert_eq!(
            backend,
            HubBackend::Remote {
                url: "http://b:8400".to_owned(),
                authoritative: false,
                hub_build: None,
                tried: urls(&["http://dead:8400", "http://b:8400"]),
            }
        );
    }

    #[test]
    fn no_candidate_answering_is_offline_not_local() {
        let candidates = urls(&["http://dead-1:8400", "http://dead-2:8400"]);
        let backend = resolve_hub(&candidates, |_| None);
        assert_eq!(
            backend,
            HubBackend::Offline {
                tried: urls(&["http://dead-1:8400", "http://dead-2:8400"]),
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
            })
        });
        match backend {
            HubBackend::Remote { authoritative, .. } => assert!(authoritative),
            other => panic!("expected Remote, got {other:?}"),
        }
    }

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
            })
        });
        assert_eq!(
            backend,
            HubBackend::Remote {
                url: "https://agentbus.dtmventures.com".to_owned(),
                authoritative: true,
                hub_build: Some("0.5.0 (cloud)".to_owned()),
                tried: urls(&["https://agentbus.dtmventures.com"]),
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
            }
        );
    }

    #[test]
    fn serializes_with_a_mode_tag_for_json_reporting() {
        let backend = HubBackend::Remote {
            url: "http://a:8400".to_owned(),
            authoritative: true,
            hub_build: Some("0.5.0 (abc123)".to_owned()),
            tried: urls(&["http://a:8400"]),
        };
        let value = serde_json::to_value(&backend).expect("serializable");
        assert_eq!(value["mode"], "remote");
        assert_eq!(value["url"], "http://a:8400");
        assert_eq!(value["authoritative"], true);
        assert_eq!(value["hub_build"], "0.5.0 (abc123)");

        let offline = HubBackend::Offline {
            tried: urls(&["http://a:8400"]),
        };
        let value = serde_json::to_value(&offline).expect("serializable");
        assert_eq!(value["mode"], "offline");

        let local = HubBackend::Local;
        let value = serde_json::to_value(&local).expect("serializable");
        assert_eq!(value["mode"], "local");
    }
}
