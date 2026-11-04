//! Input validation at the message boundary: field checks, schema handling,
//! metadata parsing and claim resource names.
#![no_main]

use agent_bus_core::channels::{
    MAX_RESOURCE_NAME_LEN, normalize_resource_name, validate_resource_name,
};
use agent_bus_core::validation::{
    MAX_BODY_LEN, MAX_TOPIC_LEN, MessageSchema, VALID_PRIORITIES, auto_fit_schema,
    enforce_schema_for_transport, infer_schema_from_topic, non_empty, parse_metadata_arg,
    reject_nul_bytes, resolve_message_schema, validate_message_schema, validate_priority,
};
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Debug, Arbitrary)]
struct Input {
    body: String,
    topic: String,
    name: String,
    schema: Option<String>,
    transport: String,
    metadata: Option<String>,
    resource: String,
    pick: u8,
}

const SCHEMAS: [&str; 3] = ["finding", "status", "benchmark"];

fuzz_target!(|input: Input| {
    // non_empty: Ok means a trimmed, non-empty, in-bounds value without NUL.
    for field in ["topic", "body", input.name.as_str()] {
        let value = if field == "topic" {
            &input.topic
        } else {
            &input.body
        };
        if let Ok(trimmed) = non_empty(value, field) {
            assert_eq!(trimmed, value.trim());
            assert!(!trimmed.is_empty());
            if field == "topic" {
                assert!(trimmed.len() <= MAX_TOPIC_LEN && !trimmed.contains('\0'));
            }
            if field == "body" {
                assert!(trimmed.len() <= MAX_BODY_LEN && !trimmed.contains('\0'));
            }
        }
    }
    assert_eq!(
        reject_nul_bytes(&input.body, "body").is_err(),
        input.body.contains('\0')
    );

    // Priority is a closed set.
    assert_eq!(
        validate_priority(&input.topic).is_ok(),
        VALID_PRIORITIES.contains(&input.topic.as_str())
    );

    // Metadata argument: the error text says "JSON object", so only an object
    // may be accepted, and it must equal what serde_json parses.
    if let Some(text) = &input.metadata {
        if let Ok(value) = parse_metadata_arg(Some(text)) {
            assert!(value.is_object(), "non-object metadata accepted: {text:?}");
            assert_eq!(
                value,
                serde_json::from_str::<serde_json::Value>(text).unwrap()
            );
        }
    }
    assert!(
        parse_metadata_arg(None)
            .expect("absent metadata")
            .is_object()
    );

    // Schemas: parse is exact; validate agrees with parse on which names exist.
    let named = input.schema.as_deref();
    if let Some(name) = named {
        assert_eq!(MessageSchema::parse(name).is_ok(), SCHEMAS.contains(&name));
        assert_eq!(
            validate_message_schema(&input.body, Some(name)).is_ok() && !SCHEMAS.contains(&name),
            false,
            "unknown schema {name:?} accepted"
        );
    }
    let _ = resolve_message_schema(&input.transport, named, &input.topic);
    let _ = enforce_schema_for_transport(&input.transport, named, &input.topic);
    let _ = infer_schema_from_topic(&input.topic, named);

    // Auto-fit: whenever it rewrites a body, the rewrite satisfies the schema.
    let schema = SCHEMAS[usize::from(input.pick) % 3];
    let fitted = auto_fit_schema(&input.body, Some(schema));
    if fitted != input.body {
        assert!(
            validate_message_schema(&fitted, Some(schema)).is_ok(),
            "auto_fit({schema}) produced a body its own validator rejects: {fitted:?}"
        );
    }

    // Claim resource names: bounded after normalisation, case-folded.
    let normalized = normalize_resource_name(&input.resource);
    if validate_resource_name(&input.resource).is_ok() {
        assert!(!normalized.is_empty());
        assert!(normalized.encode_utf16().count() <= MAX_RESOURCE_NAME_LEN);
        assert!(!normalized.contains('\\'));
    }
});
