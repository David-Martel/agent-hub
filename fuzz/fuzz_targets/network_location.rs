//! Network-location configuration and detection input: CIDR parsing, location
//! rules JSON and `resolv.conf` text.
#![no_main]

use agent_bus_core::network_location::{
    Cidr, network_fingerprint, parse_location_rules, resolv_conf_suffixes, valid_site_name,
};
use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Debug, Arbitrary)]
struct Input {
    text: String,
    v4: [u8; 4],
    other_v4: [u8; 4],
    v6: [u16; 8],
    other_v6: [u16; 8],
    prefix: u8,
    shape: Vec<u8>,
}

fn v4_range(addr: u32, prefix: u32) -> (u64, u64) {
    let size = 1u64 << (32 - prefix);
    let start = u64::from(addr) / size * size;
    (start, start + size - 1)
}

fuzz_target!(|input: Input| {
    // Arbitrary text.
    if let Ok(cidr) = Cidr::parse(&input.text) {
        // Display output re-parses to the same value (canonical form).
        assert_eq!(Cidr::parse(&cidr.to_string()), Ok(cidr), "{cidr}");
    }
    let suffixes = resolv_conf_suffixes(&input.text);
    for s in &suffixes {
        assert!(!s.is_empty() && !s.ends_with('.') && s == &s.to_ascii_lowercase());
    }

    // IPv4 membership equals the u64 interval model.
    let prefix4 = u32::from(input.prefix % 33);
    let a = Ipv4Addr::from(input.v4);
    let b = Ipv4Addr::from(input.other_v4);
    let cidr = Cidr::parse(&format!("{a}/{prefix4}")).expect("canonical v4 CIDR");
    let (lo, hi) = v4_range(u32::from(a), prefix4);
    let inside = (lo..=hi).contains(&u64::from(u32::from(b)));
    assert_eq!(cidr.contains(IpAddr::V4(b)), inside, "{b} in {cidr}");
    assert!(cidr.contains(IpAddr::V4(a)));
    assert!(
        !cidr.contains(IpAddr::V6(Ipv6Addr::from(input.v6))),
        "families never mix"
    );

    // IPv6: same address is always inside; masking is stable.
    let prefix6 = input.prefix % 129;
    let c = Ipv6Addr::from(input.v6);
    let cidr6 = Cidr::parse(&format!("{c}/{prefix6}")).expect("canonical v6 CIDR");
    assert!(cidr6.contains(IpAddr::V6(c)));
    let d = Ipv6Addr::from(input.other_v6);
    let keep = u128::MAX.checked_shl(128 - u32::from(prefix6)).unwrap_or(0);
    assert_eq!(
        cidr6.contains(IpAddr::V6(d)),
        u128::from(c) & keep == u128::from(d) & keep,
        "{d} in {cidr6}"
    );
    assert_eq!(
        Cidr::of_address(IpAddr::V4(a), input.prefix).is_some(),
        input.prefix <= 32
    );
    assert_eq!(
        Cidr::of_address(IpAddr::V6(c), input.prefix).is_some(),
        input.prefix <= 128
    );

    // Fingerprint is order-independent over its prefixes.
    let mut prefixes = vec![cidr, cidr6];
    let forward = network_fingerprint(&prefixes);
    prefixes.reverse();
    assert_eq!(
        forward,
        network_fingerprint(&prefixes),
        "fingerprint depends on order"
    );

    // Site names: the documented alphabet and length, nothing else.
    assert_eq!(
        valid_site_name(&input.text),
        !input.text.is_empty()
            && input.text.len() <= 64
            && input
                .text
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    );

    // Rules JSON built from arbitrary structure.
    let mut u = Unstructured::new(&input.shape);
    if let Ok(value) = agent_bus_fuzz::gen_json(&mut u, 3) {
        if let Ok(rules) = parse_location_rules(&value) {
            for rule in rules {
                assert!(valid_site_name(&rule.name), "rule name {:?}", rule.name);
            }
        }
    }
});
