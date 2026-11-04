//! MCP tool-call argument validation against each tool's declared input schema.
//! Accepted arguments must satisfy the schema as an independent check reads it.
#![no_main]

use agent_bus_core::mcp_dispatch::{tool_definitions, validate_tool_arguments};
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use serde_json::{Map, Value};

fn type_ok(value: &Value, expected: &str) -> bool {
    match expected {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => false,
    }
}

fuzz_target!(|data: &[u8]| {
    let mut u = Unstructured::new(data);
    let tools = tool_definitions();
    let Ok(tool) = u.choose(&tools) else { return };
    let props = tool.schema["properties"].as_object().expect("properties");
    let keys: Vec<&String> = props.keys().collect();

    // Arguments drawn mostly from the tool's own parameter names, with values of
    // arbitrary JSON type (occasionally an unknown key or an integer near a bound).
    let mut args = Map::new();
    let Ok(n) = u.int_in_range(0..=keys.len() + 2) else {
        return;
    };
    for _ in 0..n {
        let key = if keys.is_empty() || u.ratio(1, 8).unwrap_or(false) {
            match agent_bus_fuzz::gen_text(&mut u) {
                Ok(k) => k,
                Err(_) => return,
            }
        } else {
            match u.choose(&keys) {
                Ok(k) => (*k).clone(),
                Err(_) => return,
            }
        };
        let value = if u.ratio(1, 3).unwrap_or(false) {
            let edge: [i64; 6] = [0, 1, -1, i64::MAX, i64::MIN, 1_000_000];
            match u.choose(&edge) {
                Ok(v) => Value::from(*v),
                Err(_) => return,
            }
        } else {
            match agent_bus_fuzz::gen_json(&mut u, 2) {
                Ok(v) => v,
                Err(_) => return,
            }
        };
        args.insert(key, value);
    }

    if validate_tool_arguments(&tool.name, &args).is_ok() {
        for required in tool.schema["required"].as_array().into_iter().flatten() {
            let name = required.as_str().expect("required entries are strings");
            assert!(
                args.contains_key(name),
                "{}: missing required {name}",
                tool.name
            );
        }
        for (key, value) in &args {
            let schema = props
                .get(key)
                .unwrap_or_else(|| panic!("{}: unknown argument {key} accepted", tool.name));
            if let Some(expected) = schema.get("type").and_then(Value::as_str) {
                assert!(
                    type_ok(value, expected),
                    "{}: {key} is not {expected}",
                    tool.name
                );
            }
            if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
                assert!(allowed.contains(value), "{}: {key} outside enum", tool.name);
            }
            let int = value
                .as_i64()
                .map(i128::from)
                .or_else(|| value.as_u64().map(i128::from));
            if let (Some(int), Some("integer")) = (int, schema.get("type").and_then(Value::as_str))
            {
                if let Some(min) = schema.get("minimum").and_then(Value::as_i64) {
                    assert!(
                        int >= i128::from(min),
                        "{}: {key}={int} below {min}",
                        tool.name
                    );
                }
                if let Some(max) = schema.get("maximum").and_then(Value::as_i64) {
                    assert!(
                        int <= i128::from(max),
                        "{}: {key}={int} above {max}",
                        tool.name
                    );
                }
            }
        }
    }
});
