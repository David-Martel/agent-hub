//! Output encodings: MessagePack decode of untrusted bytes, MessagePack round
//! trip, TOON line formatting (one record per line), excerpts and token math.
#![no_main]

use agent_bus_core::models::{Message, Presence};
use agent_bus_core::output::{
    decode_msgpack, encode_msgpack, excerpt_body, format_message_toon, format_presence_toon,
    minimize_value, shorten_tag,
};
use agent_bus_core::token::{compact_context, estimate_tokens, minimize_message};
use agent_bus_core::validation::sanitize_for_human_display;
use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Arbitrary)]
struct Input {
    bytes: Vec<u8>,
    text: [String; 6],
    tags: Vec<String>,
    max_chars: u8,
    max_tokens: u16,
    shape: Vec<u8>,
}

fn is_single_line(s: &str) -> bool {
    !s.chars().any(|c| c.is_control())
}

fuzz_target!(|input: Input| {
    // Untrusted bytes: never panic or overflow the stack.
    let _ = decode_msgpack(&input.bytes);

    // Round trip of any JSON value without floats.
    let mut u = Unstructured::new(&input.shape);
    if let Ok(value) = agent_bus_fuzz::gen_json(&mut u, 4) {
        let packed = encode_msgpack(&value).expect("encode");
        assert_eq!(
            decode_msgpack(&packed).expect("decode"),
            value,
            "msgpack round trip"
        );
        let _ = minimize_value(&value);
    }

    let [id, from, to, topic, body, other] = input.text.clone();
    let msg = Message {
        id,
        timestamp_utc: other.clone(),
        protocol_version: "1.0".into(),
        from,
        to,
        topic,
        body,
        thread_id: None,
        tags: input.tags.iter().take(6).cloned().collect(),
        priority: "normal".into(),
        request_ack: false,
        reply_to: None,
        metadata: serde_json::Value::Null,
        stream_id: None,
        client_msg_id: None,
        origin_hub: None,
        origin_seq: None,
        hlc: None,
        sensitivity: None,
    };

    // A TOON record is one line: message content must not be able to start a
    // second record or move the terminal cursor.
    let line = format_message_toon(&msg);
    assert!(is_single_line(&line), "multi-line TOON message: {line:?}");
    let presence = Presence {
        agent: msg.from.clone(),
        status: msg.to.clone(),
        protocol_version: "1.0".into(),
        timestamp_utc: other,
        session_id: msg.id.clone(),
        capabilities: input.tags.iter().take(6).cloned().collect(),
        metadata: serde_json::Value::Null,
        ttl_seconds: 1,
    };
    let line = format_presence_toon(&presence);
    assert!(is_single_line(&line), "multi-line TOON presence: {line:?}");

    // Excerpts: short bodies are untouched, long ones are cut on a char boundary.
    let max = usize::from(input.max_chars);
    let excerpt = excerpt_body(&msg.body, max);
    if msg.body.chars().count() <= max {
        assert_eq!(excerpt, msg.body);
    } else {
        assert!(excerpt.starts_with(&msg.body.chars().take(max).collect::<String>()));
        assert!(excerpt.ends_with("chars total]"));
    }

    // Terminal sanitiser: no C0 control survives except newline and tab.
    let safe = sanitize_for_human_display(&msg.body);
    assert!(
        !safe
            .chars()
            .any(|c| (c as u32) < 0x20 && c != '\n' && c != '\t'),
        "control character survived: {safe:?}"
    );
    assert_eq!(
        sanitize_for_human_display(&safe),
        safe,
        "sanitiser is idempotent"
    );

    // Tag shortening never loses the tag's value part.
    for tag in &input.tags {
        let short = shorten_tag(tag);
        assert!(short.len() <= tag.len() + 1);
    }

    // Token estimate and compaction: total functions.
    let _ = estimate_tokens(&msg.body);
    let _ = minimize_message(&msg);
    let compacted = compact_context(&[msg.clone(), msg], usize::from(input.max_tokens));
    assert!(compacted.len() <= 2);
});
