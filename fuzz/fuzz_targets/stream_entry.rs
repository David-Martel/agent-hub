//! `decode_stream_entry`: the Redis stream entry -> `Message` decoder, including
//! transparent LZ4 body decompression and the replication-sensitivity field.
//! Entries come from a store that other writers and corruption can reach.
#![no_main]

use agent_bus_core::models::Sensitivity;
use agent_bus_core::redis_bus::decode_stream_entry;
use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use redis::Value;
use std::collections::HashMap;

const FIELDS: &[&str] = &[
    "id",
    "timestamp_utc",
    "protocol_version",
    "from",
    "to",
    "topic",
    "body",
    "thread_id",
    "tags",
    "priority",
    "request_ack",
    "reply_to",
    "metadata",
    "client_msg_id",
    "origin_hub",
    "origin_seq",
    "hlc",
    "sensitivity",
    "_compressed",
];

#[derive(Debug, Arbitrary)]
enum Raw {
    Bulk(Vec<u8>),
    Simple(String),
    Int(i64),
    Nil,
}

impl Raw {
    fn into_value(self) -> Value {
        match self {
            Raw::Bulk(b) => Value::BulkString(b),
            Raw::Simple(s) => Value::SimpleString(s),
            Raw::Int(i) => Value::Int(i),
            Raw::Nil => Value::Nil,
        }
    }
}

fn bulk(s: &str) -> Value {
    Value::BulkString(s.as_bytes().to_vec())
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);

    // 1. Arbitrary field maps of any value shape: must decode without panicking.
    let Ok(n) = u.int_in_range(0..=12) else {
        return;
    };
    let mut fields: HashMap<String, Value> = HashMap::new();
    for _ in 0..n {
        let Ok(name) = u.choose(FIELDS) else { return };
        let Ok(raw) = Raw::arbitrary(&mut u) else {
            return;
        };
        fields.insert((*name).to_string(), raw.into_value());
    }
    let _ = decode_stream_entry(&fields);

    // 2. A well-formed compressed entry returns the original body.
    let Ok(plain) = agent_bus_fuzz::gen_text(&mut u) else {
        return;
    };
    let mut entry: HashMap<String, Value> = HashMap::new();
    entry.insert("body".into(), bulk(&agent_bus_fuzz::lz4_body(&plain)));
    entry.insert("_compressed".into(), bulk("lz4"));
    assert_eq!(
        decode_stream_entry(&entry).body,
        plain,
        "lz4 body round trip"
    );

    // 3. A corrupt entry that claims a huge uncompressed size must be cheap to
    //    decode: the size prefix is attacker-controlled bytes, not a trusted length.
    let Ok(claimed) = u.arbitrary::<u32>() else {
        return;
    };
    entry.insert(
        "body".into(),
        bulk(&agent_bus_fuzz::lz4_body_claiming(&plain, claimed)),
    );
    let _ = decode_stream_entry(&entry);

    // 4. The sensitivity field fails closed: only the exact wire string
    //    "internal" (or an absent/empty field) may leave a message replicable.
    let Ok(raw) = agent_bus_fuzz::gen_text(&mut u) else {
        return;
    };
    let mut entry: HashMap<String, Value> = HashMap::new();
    entry.insert("sensitivity".into(), bulk(&raw));
    let decoded = decode_stream_entry(&entry);
    let expected = match raw.as_str() {
        "" => None,
        "internal" => Some(Sensitivity::Internal),
        _ => Some(Sensitivity::NoOffsite),
    };
    assert_eq!(decoded.sensitivity, expected, "sensitivity field {raw:?}");
    if raw != "internal" && !raw.is_empty() {
        assert_eq!(decoded.effective_sensitivity(), Sensitivity::NoOffsite);
    }
});
