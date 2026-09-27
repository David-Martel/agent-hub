//! Pure isolation-contract tests: no sockets, subprocesses, or services.
mod support;

use serde_json::{Value, json};
use std::collections::BTreeMap;
use support::Target;

fn settings() -> BTreeMap<&'static str, String> {
    [
        ("AGENT_BUS_TEST_RUN_ID", "fixture_1234"),
        ("AGENT_BUS_TEST_HTTP_URL", "http://localhost:18423"),
        ("AGENT_BUS_TEST_REDIS_URL", "redis://localhost:16423/0"),
        (
            "AGENT_BUS_TEST_DATABASE_URL",
            "postgresql://test:secret@localhost:15423/agent_bus_test_fixture_1234",
        ),
        ("AGENT_BUS_TEST_AUTH_TOKEN", "synthetic_token_for_tests"),
    ]
    .into_iter()
    .map(|(key, value)| (key, value.to_owned()))
    .collect()
}

fn target() -> Target {
    let values = settings();
    Target::parse(|key| values.get(key).cloned()).expect("valid fixture")
}

fn health() -> Value {
    json!({
        "ok": true, "database_ok": true, "storage_ready": true,
        "redis_url": "redis://localhost:16423/0",
        "database_url": "postgresql://***:***@localhost:15423/agent_bus_test_fixture_1234",
        "maintenance": {"service_agent_id": "agent-bus-test-fixture_1234"}
    })
}

#[test]
fn accepts_complete_disposable_target_and_redacted_health() {
    target()
        .validate_health(&health())
        .expect("matching services");
}

#[test]
fn recognizes_ambient_settings_independent_of_windows_environment_casing() {
    for name in [
        "AGENT_BUS_SERVER_URL",
        "agent_bus_server_url",
        "Agent_Bus_Config",
    ] {
        assert!(support::is_agent_setting(std::ffi::OsStr::new(name)));
    }
    assert!(!support::is_agent_setting(std::ffi::OsStr::new("PATH")));
}

#[test]
fn rejects_every_missing_or_empty_selector_before_io() {
    for key in settings().keys() {
        for replacement in [None, Some(String::new())] {
            let mut values = settings();
            values.remove(key);
            if let Some(value) = replacement {
                values.insert(key, value);
            }
            assert!(
                Target::parse(|name| values.get(name).cloned()).is_err(),
                "accepted missing selector {key}"
            );
        }
    }
}

#[test]
fn rejects_shared_malformed_or_ambiguous_endpoints_before_io() {
    for (key, value) in [
        ("AGENT_BUS_TEST_HTTP_URL", "http://localhost:8400"),
        ("AGENT_BUS_TEST_HTTP_URL", "http://localhost:8401"),
        ("AGENT_BUS_TEST_REDIS_URL", "redis://localhost:6380/0"),
        ("AGENT_BUS_TEST_REDIS_URL", "redis://localhost:6379/0"),
        (
            "AGENT_BUS_TEST_DATABASE_URL",
            "postgresql://localhost:5300/agent_bus_test_fixture_1234",
        ),
        (
            "AGENT_BUS_TEST_DATABASE_URL",
            "postgresql://localhost:5432/agent_bus_test_fixture_1234",
        ),
        (
            "AGENT_BUS_TEST_DATABASE_URL",
            "postgresql://localhost:15423/redis_backend",
        ),
        (
            "AGENT_BUS_TEST_DATABASE_URL",
            "postgresql://localhost:15423/agent_bus_test_different",
        ),
        ("AGENT_BUS_TEST_HTTP_URL", "http://remote:18423"),
        ("AGENT_BUS_TEST_HTTP_URL", "http://127.0.0.1:18423"),
        ("AGENT_BUS_TEST_HTTP_URL", "http://localhost"),
        ("AGENT_BUS_TEST_HTTP_URL", "http://localhost:18423/other"),
        ("AGENT_BUS_TEST_HTTP_URL", "http://user@localhost:18423"),
        (
            "AGENT_BUS_TEST_HTTP_URL",
            "http://localhost:18423?redirect=shared",
        ),
        ("AGENT_BUS_TEST_HTTP_URL", "http://localhost:18423#shared"),
        ("AGENT_BUS_TEST_HTTP_URL", "http://localhost:16423"),
        ("AGENT_BUS_TEST_REDIS_URL", "redis://localhost:16423/1"),
        ("AGENT_BUS_TEST_RUN_ID", "../production"),
        ("AGENT_BUS_TEST_RUN_ID", "short"),
        ("AGENT_BUS_TEST_AUTH_TOKEN", "short"),
        ("AGENT_BUS_TEST_AUTH_TOKEN", "invalid\r\nheader_token"),
    ] {
        let mut values = settings();
        values.insert(key, value.to_owned());
        assert!(
            Target::parse(|name| values.get(name).cloned()).is_err(),
            "accepted invalid selector {key}"
        );
    }
}

#[test]
fn fails_closed_on_unhealthy_or_missing_prerequisites() {
    for key in ["ok", "database_ok", "storage_ready"] {
        for value in [Value::Null, Value::Bool(false)] {
            let mut body = health();
            body[key] = value;
            assert!(target().validate_health(&body).is_err());
        }
    }
    assert!(target().validate_health(&json!({})).is_err());
}

#[test]
fn rejects_wrong_service_identity_and_backing_stores() {
    let mut wrong_identity = health();
    wrong_identity["maintenance"]["service_agent_id"] = json!("agent-bus");
    assert!(target().validate_health(&wrong_identity).is_err());
    for (key, value) in [
        ("redis_url", "redis://localhost:6380/0"),
        ("redis_url", "redis://localhost:16423/1"),
        ("database_url", "postgresql://localhost:15423/redis_backend"),
        (
            "database_url",
            "postgresql://remote:15423/agent_bus_test_fixture_1234",
        ),
        ("database_url", "not a URL"),
    ] {
        let mut body = health();
        body[key] = json!(value);
        assert!(target().validate_health(&body).is_err());
    }
}
