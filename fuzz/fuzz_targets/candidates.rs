//! Hub candidate configuration (`server_urls` JSON): every accepted entry must
//! satisfy the documented URL rules, and resolution must be total.
#![no_main]

use agent_bus_core::hub_candidates::{
    CandidateSpec, HubRole, build_candidates, is_public_https_target, parse_candidate_json,
    validate_candidates,
};
use agent_bus_core::network_location::valid_site_name;
use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};

#[derive(Debug, Arbitrary)]
struct Input {
    text: String,
    url: String,
    v4: [u8; 4],
    v6: [u16; 8],
    shape: Vec<u8>,
}

/// The rules `parse_hub_url` documents, restated on the raw string.
fn check_url(url: &str) {
    assert!(
        !url.chars().any(|c| c.is_whitespace() || c.is_control()),
        "whitespace/control in accepted url {url:?}"
    );
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
        .unwrap_or_else(|| panic!("accepted non-http(s) url {url:?}"));
    assert!(!url.contains('\\'), "backslash in accepted url {url:?}");
    assert!(
        !url.contains('#') && !url.contains('?'),
        "fragment or query in accepted base url {url:?}"
    );
    let authority = rest.split('/').next().unwrap_or("");
    assert!(!authority.contains('@'), "userinfo in accepted url {url:?}");
    assert!(!authority.is_empty(), "empty host in accepted url {url:?}");
}

fn check_specs(specs: &[CandidateSpec]) {
    for spec in specs {
        check_url(&spec.url);
        assert!(spec.sites.iter().all(|s| valid_site_name(s)));
        assert!(spec.hub.as_deref().is_none_or(valid_site_name));
    }
    // No two accepted entries may name the same endpoint.
    agent_bus_fuzz::assert_distinct_endpoints(specs);
}

fuzz_target!(|input: Input| {
    // Arbitrary text through the JSON entry point.
    if let Ok(specs) = parse_candidate_json(&input.text) {
        check_specs(&specs);
        let urls: Vec<String> = specs.iter().map(|s| s.url.clone()).collect();
        let by_url: BTreeMap<String, CandidateSpec> =
            specs.iter().map(|s| (s.url.clone(), s.clone())).collect();
        let candidates = build_candidates(&urls, &by_url);
        assert_eq!(candidates.len(), urls.len());
        // Resolution must be total; accepted lists keep at most one authority
        // unless they name the same hub explicitly.
        let _ = validate_candidates(&candidates);
        for (c, s) in candidates.iter().zip(&specs) {
            if let Some(role) = s.role {
                assert_eq!(c.role, role);
            }
            assert_eq!(c.sites, s.sites);
        }
    }

    // Structured: a list of objects built from a pool of pieces.
    let mut u = Unstructured::new(&input.shape);
    if let Ok(value) = agent_bus_fuzz::gen_json(&mut u, 3)
        && let Ok(specs) = agent_bus_core::hub_candidates::parse_candidate_entries(&value)
    {
        check_specs(&specs);
    }

    // Public-target classification: total on any string, and never "public"
    // for the private IPv4/IPv6 ranges it is documented to exclude.
    let _ = is_public_https_target(&input.url);
    let v4 = Ipv4Addr::from(input.v4);
    let private = v4.is_private()
        || v4.is_loopback()
        || v4.is_link_local()
        || v4.is_unspecified()
        || (input.v4[0] == 100 && (64..=127).contains(&input.v4[1]));
    assert_eq!(
        is_public_https_target(&format!("https://{v4}/")),
        !private,
        "https://{v4}/"
    );
    let v6 = Ipv6Addr::from(input.v6);
    let first = input.v6[0];
    if v6.is_loopback()
        || v6.is_unspecified()
        || first & 0xfe00 == 0xfc00
        || first & 0xffc0 == 0xfe80
    {
        assert!(
            !is_public_https_target(&format!("https://[{v6}]/")),
            "[{v6}]"
        );
    }
    let _ = HubRole::parse(&input.url);
});
