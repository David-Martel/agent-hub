//! Shared helpers for the fuzz targets.

use agent_bus_core::hub_candidates::CandidateSpec;
use arbitrary::{Result, Unstructured};
use base64::Engine as _;
use serde_json::{Map, Number, Value};

/// Short string: half the time drawn from words the code under test treats
/// specially, so coverage reaches those branches quickly.
pub fn gen_text(u: &mut Unstructured<'_>) -> Result<String> {
    const WORDS: &[&str] = &[
        "internal",
        "no-offsite",
        "sensitivity",
        "sensitivity:no-offsite",
        "finding",
        "FINDING:",
        "SEVERITY:",
        "status",
        "benchmark",
        "a=b",
        "FIX",
        "COMPLETE",
        "lz4",
        "",
    ];
    if u.ratio(1, 2)? {
        Ok((*u.choose(WORDS)?).to_string())
    } else {
        let n = u.int_in_range(0..=24)?;
        let mut s = String::new();
        for _ in 0..n {
            s.push(u.arbitrary::<char>()?);
        }
        Ok(s)
    }
}

/// Bounded random JSON value (no floats, so equality survives a codec round trip).
pub fn gen_json(u: &mut Unstructured<'_>, depth: usize) -> Result<Value> {
    // Containers only while depth remains, so nesting never exceeds `depth` and
    // stays inside MessagePack's 1024-level decode limit. At depth zero: scalars.
    let max_kind = if depth == 0 { 3 } else { 6 };
    Ok(match u.int_in_range(0..=max_kind)? {
        0 => Value::Null,
        1 => Value::Bool(u.arbitrary()?),
        2 => Value::Number(Number::from(u.arbitrary::<i64>()?)),
        3 => Value::String(gen_text(u)?),
        4 => {
            let n = u.int_in_range(0..=4)?;
            let mut items = Vec::with_capacity(n);
            for _ in 0..n {
                items.push(gen_json(u, depth.saturating_sub(1))?);
            }
            Value::Array(items)
        }
        _ => {
            let n = u.int_in_range(0..=4)?;
            let mut map = Map::new();
            for _ in 0..n {
                map.insert(gen_text(u)?, gen_json(u, depth.saturating_sub(1))?);
            }
            Value::Object(map)
        }
    })
}

/// Key under which two accepted hub URLs name the same endpoint.
///
/// Scheme and host are case-insensitive, so they are lowercased; the path is
/// case-sensitive (`/API` and `/api` are different endpoints) and kept verbatim.
/// Trailing slashes are dropped, as the parser under test drops them.
pub fn endpoint_key(url: &str) -> String {
    let url = url.trim_end_matches('/');
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    format!(
        "{}://{}{path}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    )
}

/// Panics when two accepted candidates name the same endpoint.
pub fn assert_distinct_endpoints(specs: &[CandidateSpec]) {
    for (i, a) in specs.iter().enumerate() {
        for b in &specs[i + 1..] {
            assert_ne!(
                endpoint_key(&a.url),
                endpoint_key(&b.url),
                "duplicate url accepted: {:?} and {:?}",
                a.url,
                b.url
            );
        }
    }
}

/// The stored form of a compressed body: base64(lz4 block with 4-byte size prefix).
pub fn lz4_body(plain: &str) -> String {
    base64::engine::general_purpose::STANDARD
        .encode(lz4_flex::compress_prepend_size(plain.as_bytes()))
}

/// Same, with the size prefix replaced by `claimed` (what a corrupt or hostile
/// Redis entry can carry).
pub fn lz4_body_claiming(plain: &str, claimed: u32) -> String {
    let mut block = lz4_flex::compress_prepend_size(plain.as_bytes());
    block[..4].copy_from_slice(&claimed.to_le_bytes());
    base64::engine::general_purpose::STANDARD.encode(block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_bus_core::hub_candidates::{CandidateAuth, parse_candidate_json};
    use agent_bus_core::output::{decode_msgpack, encode_msgpack};

    /// Run `f` on a thread with a large stack, so a deep value can fail on its
    /// own terms (a decode error) instead of overflowing the harness thread.
    fn on_big_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::Builder::new()
            .stack_size(256 << 20)
            .spawn(f)
            .expect("spawn")
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    }

    fn nesting(value: &Value) -> usize {
        match value {
            Value::Array(items) => 1 + items.iter().map(nesting).max().unwrap_or(0),
            Value::Object(map) => 1 + map.values().map(nesting).max().unwrap_or(0),
            _ => 0,
        }
    }

    fn spec(url: &str) -> CandidateSpec {
        CandidateSpec {
            url: url.to_owned(),
            role: None,
            auth: CandidateAuth::Global,
            sites: Vec::new(),
            hub: None,
        }
    }

    #[test]
    fn depth_zero_generates_only_scalars() {
        for byte in 0..=u8::MAX {
            let data = [byte; 64];
            let value = gen_json(&mut Unstructured::new(&data), 0).expect("generate");
            assert_eq!(nesting(&value), 0, "byte {byte} produced {value}");
        }
    }

    #[test]
    fn nesting_never_exceeds_the_requested_depth() {
        on_big_stack(|| {
            // 0x04 selects "array of four" at every level, the deepest the
            // generator can be driven; 4096 bytes is far more than a limit's worth.
            let data = [4u8; 4096];
            for depth in 0..=5 {
                let value = gen_json(&mut Unstructured::new(&data), depth).expect("generate");
                assert!(
                    nesting(&value) <= depth,
                    "depth {depth} produced nesting {}",
                    nesting(&value)
                );
            }
        });
    }

    #[test]
    fn deep_input_still_round_trips_through_msgpack() {
        on_big_stack(|| {
            let data = [4u8; 4096];
            let value = gen_json(&mut Unstructured::new(&data), 4).expect("generate");
            eprintln!("generated nesting: {}", nesting(&value));
            let packed = encode_msgpack(&value).expect("encode");
            // MessagePack decoding rejects more than 1024 levels, so a value this
            // generator emits must stay well inside that.
            let decoded = decode_msgpack(&packed).expect("decode within the depth limit");
            assert_eq!(decoded, value);
        });
    }

    #[test]
    fn candidate_paths_are_case_sensitive() {
        let specs =
            parse_candidate_json(r#"["https://hub.example/API","https://hub.example/api"]"#)
                .expect("the parser keeps case-distinct paths");
        assert_eq!(specs.len(), 2);
        assert_distinct_endpoints(&specs);
    }

    #[test]
    fn endpoint_key_folds_scheme_and_host_only() {
        assert_eq!(
            endpoint_key("HTTPS://Hub.Example:8443/Api/"),
            endpoint_key("https://hub.example:8443/Api")
        );
        assert_ne!(
            endpoint_key("https://hub.example/API"),
            endpoint_key("https://hub.example/api")
        );
        assert_eq!(endpoint_key("https://Hub.Example/"), "https://hub.example");
    }

    #[test]
    #[should_panic(expected = "duplicate url accepted")]
    fn the_same_endpoint_twice_is_still_caught() {
        assert_distinct_endpoints(&[
            spec("https://hub.example/api"),
            spec("HTTPS://HUB.example/api/"),
        ]);
    }
}
