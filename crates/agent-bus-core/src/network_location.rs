//! Network-location-aware hub candidate ordering (agent-hub#79 follow-up).
//!
//! Operator laptops roam between the lab fabric, the campus network (or its
//! VPN) and home networks. Each location has a different working path to the
//! same on-site hub: a direct fabric address on the bench, an SSH-forwarded
//! loopback port from campus, and only the cloud tier from home. Probing
//! every candidate in one fixed order makes a roaming client wait out dead
//! paths first on every invocation.
//!
//! This module detects the client's current location from cheap local
//! signals (interface addresses and connection-specific DNS suffixes) and
//! reorders candidates so that the ones declared for that location are
//! probed first. It never changes a candidate's role: authority, credentials
//! and fail-closed behavior stay exactly as configured. Only probe ORDER
//! changes.
//!
//! The site names and their matching networks live in configuration
//! (`network_locations` in `config.json`), never in source:
//!
//! ```json
//! "network_locations": [
//!   {"name": "lab-fabric", "cidrs": ["10.60.0.0/16"]},
//!   {"name": "campus", "dns_suffixes": ["example.edu"]},
//!   {"name": "home", "dns_suffixes": ["home.example.com"]}
//! ]
//! ```
//!
//! `AGENT_BUS_NETWORK_LOCATION=name[,name...]` (or `none`) overrides
//! detection for tests and unusual networks.

use std::net::IpAddr;

use serde_json::Value;

use crate::hub_candidates::HubCandidate;

/// Environment variable that replaces detection with an explicit site list.
pub const LOCATION_OVERRIDE_ENV: &str = "AGENT_BUS_NETWORK_LOCATION";

/// An IP prefix such as `10.60.0.0/16` or `fd00::/8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    /// Parse `address/prefix`. The address is masked to the prefix.
    ///
    /// # Errors
    /// A message when the address, the prefix, or their combination is invalid.
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (address, prefix) = raw
            .trim()
            .split_once('/')
            .ok_or_else(|| format!("cidr {raw:?} must be address/prefix"))?;
        let network: IpAddr = address
            .parse()
            .map_err(|_error| format!("cidr {raw:?} has an invalid address"))?;
        let prefix: u8 = prefix
            .parse()
            .map_err(|_error| format!("cidr {raw:?} has an invalid prefix"))?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        if prefix > max {
            return Err(format!("cidr {raw:?} prefix exceeds {max}"));
        }
        Ok(Self {
            network: mask(network, prefix),
            prefix,
        })
    }

    /// `true` when `address` falls inside this prefix (same family only).
    #[must_use]
    pub fn contains(&self, address: IpAddr) -> bool {
        address.is_ipv4() == self.network.is_ipv4() && mask(address, self.prefix) == self.network
    }
}

fn mask(address: IpAddr, prefix: u8) -> IpAddr {
    match address {
        IpAddr::V4(v4) => {
            let bits = u32::from(v4);
            let keep = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V4((bits & keep).into())
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6);
            let keep = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V6((bits & keep).into())
        }
    }
}

/// One named location and the signals that identify it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocationRule {
    /// The site name candidates refer to in their `sites` list.
    pub name: String,
    /// Any active interface address inside one of these prefixes matches.
    pub cidrs: Vec<Cidr>,
    /// Any active connection DNS suffix equal to, or ending in, one of these
    /// domains matches (ASCII case-insensitive).
    pub dns_suffixes: Vec<String>,
}

/// `true` for a usable site name: 1–64 chars of `[A-Za-z0-9._-]`.
#[must_use]
pub fn valid_site_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

fn normalize_domain(raw: &str) -> String {
    raw.trim().trim_end_matches('.').to_ascii_lowercase()
}

fn string_list(fields: &serde_json::Map<String, Value>, key: &str) -> Result<Vec<String>, String> {
    match fields.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| format!("{key} entries must be strings"))
            })
            .collect(),
        Some(_) => Err(format!("{key} must be an array of strings")),
    }
}

/// Parse the `network_locations` configuration value.
///
/// # Errors
/// A message naming the first invalid rule. Unknown keys, duplicate names,
/// and rules with no signal at all are rejected.
pub fn parse_location_rules(value: &Value) -> Result<Vec<LocationRule>, String> {
    let entries = value
        .as_array()
        .ok_or("network_locations must be an array")?;
    let mut rules: Vec<LocationRule> = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let position = index + 1;
        let fields = entry
            .as_object()
            .ok_or_else(|| format!("network location {position} must be an object"))?;
        for key in fields.keys() {
            if !matches!(key.as_str(), "name" | "cidrs" | "dns_suffixes") {
                return Err(format!("network location {position}: unknown key {key:?}"));
            }
        }
        let name = fields
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| valid_site_name(name))
            .ok_or_else(|| {
                format!("network location {position}: name must match [A-Za-z0-9._-]{{1,64}}")
            })?
            .to_owned();
        if rules.iter().any(|rule| rule.name == name) {
            return Err(format!(
                "network location {position}: duplicate name {name:?}"
            ));
        }
        let cidrs: Vec<Cidr> = string_list(fields, "cidrs")
            .and_then(|raw| raw.iter().map(|cidr| Cidr::parse(cidr)).collect())
            .map_err(|error| format!("network location {position}: {error}"))?;
        let dns_suffixes = string_list(fields, "dns_suffixes")
            .map_err(|error| format!("network location {position}: {error}"))?
            .iter()
            .map(|suffix| normalize_domain(suffix))
            .collect::<Vec<_>>();
        if dns_suffixes.iter().any(String::is_empty) {
            return Err(format!("network location {position}: empty dns suffix"));
        }
        if cidrs.is_empty() && dns_suffixes.is_empty() {
            return Err(format!(
                "network location {position}: needs at least one cidr or dns suffix"
            ));
        }
        rules.push(LocationRule {
            name,
            cidrs,
            dns_suffixes,
        });
    }
    Ok(rules)
}

/// What the local machine currently looks like on the network.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkSignals {
    /// Non-loopback addresses on interfaces that are operationally up.
    pub addresses: Vec<IpAddr>,
    /// Connection-specific DNS suffixes / search domains of active links.
    pub dns_suffixes: Vec<String>,
}

/// Source of network signals and of the explicit override, injectable for tests.
pub trait NetworkProbe {
    /// Collect current signals. Failures yield empty lists, never errors:
    /// detection only reorders, so "unknown" is always safe.
    fn signals(&self) -> NetworkSignals;
    /// The raw [`LOCATION_OVERRIDE_ENV`] value, if set.
    fn override_value(&self) -> Option<String>;
}

/// Names of every rule the signals match, in rule order.
#[must_use]
pub fn detect_locations(rules: &[LocationRule], signals: &NetworkSignals) -> Vec<String> {
    let suffixes: Vec<String> = signals
        .dns_suffixes
        .iter()
        .map(|suffix| normalize_domain(suffix))
        .filter(|suffix| !suffix.is_empty())
        .collect();
    rules
        .iter()
        .filter(|rule| {
            rule.cidrs.iter().any(|cidr| {
                signals
                    .addresses
                    .iter()
                    .any(|address| cidr.contains(*address))
            }) || rule.dns_suffixes.iter().any(|wanted| {
                suffixes.iter().any(|seen| {
                    seen == wanted
                        || seen
                            .strip_suffix(wanted.as_str())
                            .is_some_and(|head| head.ends_with('.'))
                })
            })
        })
        .map(|rule| rule.name.clone())
        .collect()
}

/// The detected location set, or `None` when location-aware ordering is off
/// (no rules configured and no override set). `Some(vec![])` means "aware,
/// but no known site matched".
#[must_use]
pub fn current_locations(rules: &[LocationRule], probe: &dyn NetworkProbe) -> Option<Vec<String>> {
    if let Some(raw) = probe.override_value() {
        let raw = raw.trim();
        if !raw.is_empty() {
            if raw.eq_ignore_ascii_case("none") {
                return Some(Vec::new());
            }
            let mut names: Vec<String> = Vec::new();
            for name in raw.split(',').map(str::trim) {
                if valid_site_name(name) && !names.iter().any(|seen| seen == name) {
                    names.push(name.to_owned());
                }
            }
            return Some(names);
        }
    }
    if rules.is_empty() {
        return None;
    }
    Some(detect_locations(rules, &probe.signals()))
}

/// Probe order for `candidates` at `locations`.
///
/// Candidates whose `sites` include a current location move to the front;
/// every other candidate follows in configured order. With `None`
/// (location-unaware) or no match, the configured order is unchanged, so a
/// wrong or spoofed detection (DNS suffixes come from DHCP) can only promote
/// a route the operator declared for that site; it never demotes the
/// configured preference among the rest.
#[must_use]
pub fn location_order(candidates: &[HubCandidate], locations: Option<&[String]>) -> Vec<usize> {
    let Some(locations) = locations else {
        return (0..candidates.len()).collect();
    };
    let rank = |candidate: &HubCandidate| {
        u8::from(
            !candidate
                .sites
                .iter()
                .any(|site| locations.iter().any(|location| location == site)),
        )
    };
    let mut order: Vec<usize> = (0..candidates.len()).collect();
    order.sort_by_key(|&index| rank(&candidates[index]));
    order
}

/// The real machine: interface addresses plus OS DNS-suffix configuration.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemNetwork;

impl NetworkProbe for SystemNetwork {
    fn signals(&self) -> NetworkSignals {
        let addresses: Vec<IpAddr> = if_addrs::get_if_addrs()
            .map(|interfaces| {
                interfaces
                    .into_iter()
                    .filter(|interface| interface.is_oper_up() && !interface.is_loopback())
                    .map(|interface| interface.ip())
                    .collect()
            })
            .unwrap_or_default();
        let dns_suffixes = system_dns_suffixes(&addresses);
        NetworkSignals {
            addresses,
            dns_suffixes,
        }
    }

    fn override_value(&self) -> Option<String> {
        std::env::var(LOCATION_OVERRIDE_ENV).ok()
    }
}

/// Parse `search`/`domain` lines of a resolv.conf-format file.
#[must_use]
pub fn resolv_conf_suffixes(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter_map(|line| {
            line.strip_prefix("search")
                .or_else(|| line.strip_prefix("domain"))
                .filter(|rest| rest.starts_with(char::is_whitespace))
        })
        .flat_map(str::split_whitespace)
        .map(normalize_domain)
        .filter(|domain| !domain.is_empty())
        .collect()
}

#[cfg(not(windows))]
fn system_dns_suffixes(_active: &[IpAddr]) -> Vec<String> {
    let mut suffixes = Vec::new();
    for path in ["/run/systemd/resolve/resolv.conf", "/etc/resolv.conf"] {
        if let Ok(text) = std::fs::read_to_string(path) {
            for suffix in resolv_conf_suffixes(&text) {
                if !suffixes.contains(&suffix) {
                    suffixes.push(suffix);
                }
            }
        }
    }
    suffixes
}

/// Windows keeps per-interface `Domain` / `DhcpDomain` values under the
/// Tcpip parameters key, including for links that are down. Only interfaces
/// whose configured or leased address is currently active contribute, so a
/// stale home lease never claims "home" while on campus.
#[cfg(windows)]
fn system_dns_suffixes(active: &[IpAddr]) -> Vec<String> {
    use winreg::RegKey;
    use winreg::enums::HKEY_LOCAL_MACHINE;

    const INTERFACES: &str = r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces";
    let Ok(root) = RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey(INTERFACES) else {
        return Vec::new();
    };
    let mut suffixes = Vec::new();
    for name in root.enum_keys().flatten() {
        let Ok(interface) = root.open_subkey(&name) else {
            continue;
        };
        let mut addresses: Vec<String> = Vec::new();
        if let Ok(leased) = interface.get_value::<String, _>("DhcpIPAddress") {
            addresses.push(leased);
        }
        if let Ok(fixed) = interface.get_value::<Vec<String>, _>("IPAddress") {
            addresses.extend(fixed);
        }
        let live = addresses
            .iter()
            .filter_map(|raw| raw.trim().parse::<IpAddr>().ok())
            .any(|address| active.contains(&address));
        if !live {
            continue;
        }
        for value in ["Domain", "DhcpDomain"] {
            if let Ok(raw) = interface.get_value::<String, _>(value) {
                let domain = normalize_domain(&raw);
                if !domain.is_empty() && !suffixes.contains(&domain) {
                    suffixes.push(domain);
                }
            }
        }
    }
    suffixes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub_candidates::{CandidateAuth, HubRole};
    use serde_json::json;

    struct FakeNetwork {
        signals: NetworkSignals,
        override_value: Option<String>,
    }

    impl NetworkProbe for FakeNetwork {
        fn signals(&self) -> NetworkSignals {
            self.signals.clone()
        }
        fn override_value(&self) -> Option<String> {
            self.override_value.clone()
        }
    }

    fn network(addresses: &[&str], suffixes: &[&str]) -> FakeNetwork {
        FakeNetwork {
            signals: NetworkSignals {
                addresses: addresses
                    .iter()
                    .map(|raw| raw.parse().expect("address"))
                    .collect(),
                dns_suffixes: suffixes.iter().map(|raw| (*raw).to_owned()).collect(),
            },
            override_value: None,
        }
    }

    fn rules() -> Vec<LocationRule> {
        parse_location_rules(&json!([
            {"name": "lab-fabric", "cidrs": ["10.60.0.0/16"]},
            {"name": "campus", "dns_suffixes": ["Example.EDU."]},
            {"name": "home", "dns_suffixes": ["home.example.com"]}
        ]))
        .expect("rules")
    }

    fn candidate(url: &str, role: HubRole, sites: &[&str]) -> HubCandidate {
        HubCandidate {
            url: url.to_owned(),
            role,
            auth: CandidateAuth::Global,
            sites: sites.iter().map(|site| (*site).to_owned()).collect(),
            hub: None,
        }
    }

    fn fleet() -> Vec<HubCandidate> {
        vec![
            candidate(
                "http://fabric.hub:8400",
                HubRole::Authoritative,
                &["lab-fabric"],
            ),
            candidate("http://127.0.0.1:18400", HubRole::Fallback, &["campus"]),
            candidate("https://cloud.example.com", HubRole::Cloud, &[]),
        ]
    }

    #[test]
    fn cidr_parsing_masks_and_matches_by_family() {
        let cidr = Cidr::parse("10.60.4.9/16").expect("cidr");
        assert!(cidr.contains("10.60.250.1".parse().expect("ip")));
        assert!(!cidr.contains("10.61.0.1".parse().expect("ip")));
        assert!(!cidr.contains("::ffff:10.60.0.1".parse().expect("ip")));
        assert!(
            Cidr::parse("0.0.0.0/0")
                .expect("any")
                .contains("8.8.8.8".parse().expect("ip"))
        );
        let v6 = Cidr::parse("fd00::/8").expect("v6");
        assert!(v6.contains("fd12::1".parse().expect("ip")));
        for bad in ["10.0.0.0", "10.0.0.0/33", "x/8", "::/129", "10.0.0.0/-1"] {
            assert!(Cidr::parse(bad).is_err(), "{bad} must be rejected");
        }
    }

    #[test]
    fn rule_parsing_rejects_ambiguous_or_empty_rules() {
        for bad in [
            json!({}),
            json!([{"name": "x"}]),
            json!([{"name": "", "cidrs": ["10.0.0.0/8"]}]),
            json!([{"name": "has space", "cidrs": ["10.0.0.0/8"]}]),
            json!([{"name": "x", "cidrs": ["10.0.0.0/8"], "extra": 1}]),
            json!([{"name": "x", "cidrs": "10.0.0.0/8"}]),
            json!([{"name": "x", "dns_suffixes": [" "]}]),
            json!([{"name": "x", "cidrs": ["10.0.0.0/8"]}, {"name": "x", "cidrs": ["10.1.0.0/16"]}]),
        ] {
            assert!(
                parse_location_rules(&bad).is_err(),
                "{bad} must be rejected"
            );
        }
        assert_eq!(rules()[1].dns_suffixes, vec!["example.edu".to_owned()]);
    }

    #[test]
    fn detection_matches_cidrs_and_dns_suffix_boundaries() {
        let rules = rules();
        let fabric = network(&["10.60.4.5"], &[]);
        assert_eq!(
            current_locations(&rules, &fabric),
            Some(vec!["lab-fabric".to_owned()])
        );
        let roaming = network(
            &["35.7.14.208", "10.10.15.38"],
            &["ads.Example.edu", "lan.home.example.com."],
        );
        assert_eq!(
            current_locations(&rules, &roaming),
            Some(vec!["campus".to_owned(), "home".to_owned()])
        );
        // A suffix match needs a label boundary: "notexample.edu" is not "example.edu".
        let lookalike = network(&["192.0.2.1"], &["notexample.edu"]);
        assert_eq!(current_locations(&rules, &lookalike), Some(Vec::new()));
    }

    #[test]
    fn detection_is_off_without_rules_or_override() {
        let probe = network(&["10.60.4.5"], &["example.edu"]);
        assert_eq!(current_locations(&[], &probe), None);
    }

    #[test]
    fn override_replaces_detection_even_without_rules() {
        let mut probe = network(&["10.60.4.5"], &[]);
        probe.override_value = Some(" campus, campus ,bad name, home ".to_owned());
        assert_eq!(
            current_locations(&[], &probe),
            Some(vec!["campus".to_owned(), "home".to_owned()])
        );
        probe.override_value = Some("NONE".to_owned());
        assert_eq!(current_locations(&rules(), &probe), Some(Vec::new()));
        probe.override_value = Some("   ".to_owned());
        assert_eq!(
            current_locations(&rules(), &probe),
            Some(vec!["lab-fabric".to_owned()])
        );
    }

    #[test]
    fn unaware_ordering_is_exactly_the_configured_order() {
        assert_eq!(location_order(&fleet(), None), vec![0, 1, 2]);
    }

    #[test]
    fn matching_sites_first_then_configured_order() {
        let campus = vec!["campus".to_owned()];
        assert_eq!(location_order(&fleet(), Some(&campus)), vec![1, 0, 2]);
        let fabric = vec!["lab-fabric".to_owned()];
        assert_eq!(location_order(&fleet(), Some(&fabric)), vec![0, 1, 2]);
        // Nothing matched: the configured preference is kept exactly, so an
        // unknown or spoofed network cannot demote a reachable authority.
        assert_eq!(location_order(&fleet(), Some(&[])), vec![0, 1, 2]);
        let both = vec!["campus".to_owned(), "lab-fabric".to_owned()];
        assert_eq!(location_order(&fleet(), Some(&both)), vec![0, 1, 2]);
    }

    #[test]
    fn location_never_changes_roles_only_order() {
        let candidates = fleet();
        for locations in [
            None,
            Some(vec![]),
            Some(vec!["campus".to_owned()]),
            Some(vec!["lab-fabric".to_owned(), "home".to_owned()]),
        ] {
            let order = location_order(&candidates, locations.as_deref());
            let mut sorted = order.clone();
            sorted.sort_unstable();
            assert_eq!(
                sorted,
                vec![0, 1, 2],
                "every candidate is probed exactly once"
            );
            let authorities: Vec<&str> = order
                .iter()
                .map(|&index| &candidates[index])
                .filter(|candidate| candidate.role == HubRole::Authoritative)
                .map(|candidate| candidate.url.as_str())
                .collect();
            assert_eq!(authorities, vec!["http://fabric.hub:8400"]);
        }
    }

    #[test]
    fn resolv_conf_search_and_domain_lines() {
        let text = "# comment\nnameserver 127.0.0.53\nsearch lab.example.edu. Home.Example.com\ndomain   corp.example\nsearchx nope\noptions edns0\n";
        assert_eq!(
            resolv_conf_suffixes(text),
            vec![
                "lab.example.edu".to_owned(),
                "home.example.com".to_owned(),
                "corp.example".to_owned()
            ]
        );
    }

    #[test]
    fn system_probe_does_not_panic() {
        let signals = SystemNetwork.signals();
        assert!(
            signals
                .addresses
                .iter()
                .all(|address| !address.is_loopback())
        );
    }
}
