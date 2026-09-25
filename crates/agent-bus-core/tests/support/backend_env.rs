//! Backend addressing for the `#[ignore]`d backend integration tests.
//!
//! Included by path (`#[path = ...] mod backend_env;`) from every backend test
//! file in the workspace so there is one definition of the rules below.
//!
//! Two rules, both enforced by panicking rather than skipping:
//!
//! 1. **Backends are never defaulted.** A backend test reads its Redis,
//!    `PostgreSQL` and HTTP server URLs from `AGENT_BUS_TEST_REDIS_URL`,
//!    `AGENT_BUS_TEST_DATABASE_URL` and `AGENT_BUS_TEST_SERVER_URL`. An unset
//!    variable fails the test. The previous behaviour -- probe the agent-bus
//!    default ports and return early when nothing answered -- reported a pass
//!    for a test that exercised nothing, and on a host running the real bus it
//!    silently ran against production.
//! 2. **The live bus ports are refused.** 6380 (Redis), 5300 (`PostgreSQL`) and
//!    8400 (HTTP) are the fleet's production ports. A URL naming one of them is
//!    rejected even when set explicitly, so a copied production DSN cannot turn
//!    a test run into writes against the live coordination bus.
//!
//! Run the backend tests with `cargo test --workspace --tests -- --ignored`
//! after starting disposable backends on other ports (CI's
//! `test-integration` job does exactly this).

// Each including test binary uses a different subset of these helpers.
#![allow(
    dead_code,
    reason = "shared by path across test binaries with different needs"
)]

/// Ports the live agent-bus backend and hub listen on. Tests must never use them.
pub const LIVE_BUS_PORTS: [u16; 3] = [6380, 5300, 8400];

pub const REDIS_URL_VAR: &str = "AGENT_BUS_TEST_REDIS_URL";
pub const DATABASE_URL_VAR: &str = "AGENT_BUS_TEST_DATABASE_URL";
pub const SERVER_URL_VAR: &str = "AGENT_BUS_TEST_SERVER_URL";

/// Return the backend URL held in `var`, panicking when it is unset, empty,
/// or names a live bus port.
pub fn backend_url(var: &str) -> String {
    checked_backend_url(var, std::env::var(var).ok())
}

/// The rules of [`backend_url`] applied to an explicit value, so they can be
/// tested without mutating the process environment.
pub fn checked_backend_url(var: &str, value: Option<String>) -> String {
    let url = value.unwrap_or_default();
    assert!(
        !url.trim().is_empty(),
        "{var} is not set. Backend tests never fall back to a default address; \
         start disposable backends on non-live ports and export {var} \
         (run with `cargo test -- --ignored`)."
    );
    if let Some(port) = url_port(&url) {
        assert!(
            !LIVE_BUS_PORTS.contains(&port),
            "{var}={url} targets port {port}, a live agent-bus port \
             ({LIVE_BUS_PORTS:?}). Backend tests refuse to run against the live bus."
        );
    }
    url
}

/// Formats as the HTTP server URL from `AGENT_BUS_TEST_SERVER_URL` (without a
/// trailing slash), so HTTP tests can keep `format!("{BASE_URL}/health")` while
/// the address comes from the environment -- and panics there when it is unset.
#[derive(Debug, Clone, Copy)]
pub struct TestServerUrl;

impl std::fmt::Display for TestServerUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(backend_url(SERVER_URL_VAR).trim_end_matches('/'))
    }
}

/// Extract the explicit port from `scheme://[user@]host:port[/path]`.
/// Returns `None` when the URL carries no explicit port.
pub fn url_port(url: &str) -> Option<u16> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, hp)| hp);
    let port = if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split_once("]:").map(|(_, port)| port)
    } else {
        host_port.rsplit_once(':').map(|(_, port)| port)
    };
    port.and_then(|p| p.parse().ok())
}

#[test]
fn url_port_parses_the_shapes_backend_tests_use() {
    assert_eq!(url_port("redis://127.0.0.1:16380/0"), Some(16380));
    assert_eq!(
        url_port("postgresql://postgres:pw@localhost:15300/db"),
        Some(15300)
    );
    assert_eq!(url_port("http://[::1]:18400"), Some(18400));
    assert_eq!(url_port("http://localhost"), None);
}

#[test]
fn backend_url_refuses_every_live_bus_port() {
    // Negative control for rule 2: each live port must be refused on its own,
    // in every URL shape the tests use, and a non-live port must be accepted.
    for port in LIVE_BUS_PORTS {
        for url in [
            format!("redis://127.0.0.1:{port}/0"),
            format!("postgresql://postgres@localhost:{port}/db"),
            format!("http://[::1]:{port}"),
        ] {
            let refused =
                std::panic::catch_unwind(|| checked_backend_url("PROBE", Some(url.clone())));
            assert!(refused.is_err(), "live port URL was accepted: {url}");
        }
    }
    assert_eq!(
        checked_backend_url("PROBE", Some("redis://127.0.0.1:16380/0".to_owned())),
        "redis://127.0.0.1:16380/0"
    );
}

#[test]
#[should_panic(expected = "is not set")]
fn backend_url_refuses_an_unset_variable() {
    let _ = checked_backend_url("PROBE", None);
}

#[test]
#[should_panic(expected = "is not set")]
fn backend_url_refuses_an_empty_variable() {
    let _ = checked_backend_url("PROBE", Some("  ".to_owned()));
}
