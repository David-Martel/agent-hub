//! Shared runtime for agent-bus: models, settings, storage, ops, token, output.

use std::sync::OnceLock;

pub mod agent_profile;
pub mod channels;
pub mod codex_bridge;
pub mod journal;
pub mod mcp_dispatch;
pub mod models;
pub mod ops;
pub mod output;
pub mod postgres_store;
pub mod redis_bus;
pub mod settings;
pub mod token;
pub mod validation;

use models::STARTUP_PRESENCE_TTL;
use ops::{PostMessageRequest, PresenceRequest, post_message, set_presence};
use postgres_store::{PgWriter, probe_postgres};
use redis_bus::connect;
use settings::Settings;

static PG_WRITER: OnceLock<PgWriter> = OnceLock::new();

/// Returns a reference to the process-lifetime [`PgWriter`] singleton, or
/// `None` if it has not yet been initialised.
#[must_use]
pub fn pg_writer() -> Option<&'static PgWriter> {
    PG_WRITER.get()
}

/// Initialise the process-lifetime [`PgWriter`] singleton.
///
/// Must be called at most once before any code path that calls [`pg_writer`].
/// Returns `Err` if called more than once.
///
/// # Errors
/// Returns `Err(writer)` if the singleton was already set.
pub fn init_pg_writer(writer: PgWriter) -> Result<(), PgWriter> {
    PG_WRITER.set(writer)
}

/// Announce this process on the bus if `settings.startup_enabled` is true.
///
/// Posts a presence record and a startup message so that other agents can
/// discover that this node is online. Silently returns if the Redis
/// connection fails or if startup announcements are disabled.
pub fn maybe_announce_startup(settings: &Settings) {
    if !settings.startup_enabled {
        return;
    }
    let Ok(mut conn) = connect(settings) else {
        return;
    };
    let meta = serde_json::json!({"service": "agent-bus", "startup": true});
    let mut caps = vec!["mcp".to_owned(), "redis".to_owned()];
    if probe_postgres(settings).0 == Some(true) {
        caps.push("postgres".to_owned());
    }
    let _ = set_presence(
        &mut conn,
        settings,
        &PresenceRequest {
            agent: &settings.service_agent_id,
            status: "online",
            session_id: None,
            capabilities: &caps,
            ttl_seconds: STARTUP_PRESENCE_TTL,
            metadata: &meta,
        },
    );
    let startup_tags = [
        "startup".to_owned(),
        "system".to_owned(),
        "health".to_owned(),
    ];
    let _ = post_message(
        &mut conn,
        settings,
        &PostMessageRequest {
            sender: &settings.service_agent_id,
            recipient: &settings.startup_recipient,
            topic: &settings.startup_topic,
            body: &settings.startup_body,
            thread_id: None,
            tags: &startup_tags,
            priority: "normal",
            request_ack: false,
            reply_to: None,
            metadata: &meta,
            has_sse_subscribers: false,
        },
    );
}
