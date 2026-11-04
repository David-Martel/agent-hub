//! Shared helpers for the fuzz targets.

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
    let max_kind = if depth == 0 { 4 } else { 6 };
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
