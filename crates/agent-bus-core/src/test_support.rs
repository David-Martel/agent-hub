//! Backend addressing for this crate's own unit tests (`cfg(test)` only).
//!
//! Unit tests used `Settings::from_env()`, which resolves to the live bus
//! (`127.0.0.1:6380` / `:5300`) on any host that runs it -- and the "accepts
//! valid input" tests then carried on and wrote to production (created a
//! group, subscribed agent `claude`, pushed task cards to `codex`). Now:
//!
//! - Tests that must not reach a backend use [`offline_settings`], whose
//!   Redis/PostgreSQL URLs point at a closed port, so any I/O fails with a
//!   connection error after validation has run.
//! - Tests that need a backend are `#[ignore = "backend test: ..."]` (the test
//!   harness counts them as ignored when unconfigured) and use
//!   [`backend_settings`], which panics when `AGENT_BUS_TEST_*` is unset or
//!   names a live port. CI's `test-integration` job runs them.
//! - [`refuse_live_bus`] is called by every connection path in this crate and,
//!   in unit-test builds only, panics on a live-bus port -- so a new test that
//!   falls back to the defaults fails on every host, including CI.

#[path = "../tests/support/backend_env.rs"]
mod backend_env;

pub(crate) use backend_env::{
    DATABASE_URL_VAR, LIVE_BUS_PORTS, REDIS_URL_VAR, backend_url, url_port,
};

use crate::settings::Settings;

/// A port nothing listens on: connections are refused immediately.
const CLOSED_PORT_REDIS_URL: &str = "redis://127.0.0.1:1/0";
const CLOSED_PORT_DATABASE_URL: &str = "postgresql://postgres@127.0.0.1:1/offline";

/// [`Settings`] whose backends are unreachable by construction.
pub(crate) fn offline_settings() -> Settings {
    let mut settings = Settings::from_env();
    CLOSED_PORT_REDIS_URL.clone_into(&mut settings.redis_url);
    settings.database_url = Some(CLOSED_PORT_DATABASE_URL.to_owned());
    settings
}

/// [`Settings`] pointed at the disposable test backends. Panics when a
/// variable is unset or names a live port.
pub(crate) fn backend_settings() -> Settings {
    let mut settings = Settings::from_env();
    settings.redis_url = backend_url(REDIS_URL_VAR);
    settings.database_url = Some(backend_url(DATABASE_URL_VAR));
    settings
}

/// Panic if `url` targets a live agent-bus port. Called by every connection
/// path in this crate; compiled only into this crate's unit tests.
pub(crate) fn refuse_live_bus(url: &str) {
    if let Some(port) = url_port(url) {
        assert!(
            !LIVE_BUS_PORTS.contains(&port),
            "unit test tried to connect to live agent-bus port {port} ({url}). \
             Use test_support::offline_settings() or, for a backend test, \
             #[ignore] + test_support::backend_settings()."
        );
    }
}

#[test]
fn refuse_live_bus_panics_on_every_live_port_and_passes_others() {
    for port in LIVE_BUS_PORTS {
        let url = format!("redis://127.0.0.1:{port}/0");
        assert!(
            std::panic::catch_unwind(|| refuse_live_bus(&url)).is_err(),
            "{url} was not refused"
        );
    }
    refuse_live_bus(CLOSED_PORT_REDIS_URL);
    refuse_live_bus(CLOSED_PORT_DATABASE_URL);
}

#[test]
fn default_settings_are_refused_by_the_connection_guard() {
    // Negative control for the guard: a unit test that connects with the
    // compiled-in defaults (no env, no config) must panic, not reach the bus.
    let defaults = [
        "redis://127.0.0.1:6380/0",
        "postgresql://postgres@127.0.0.1:5300/redis_backend",
    ];
    for url in defaults {
        assert!(std::panic::catch_unwind(|| refuse_live_bus(url)).is_err());
    }
    let err = std::panic::catch_unwind(|| {
        let mut s = offline_settings();
        s.redis_url = "redis://127.0.0.1:6380/0".to_owned();
        crate::redis_bus::connect(&s).map(|_| ())
    });
    assert!(
        err.is_err(),
        "connect() with a live port must panic in unit tests"
    );
}
