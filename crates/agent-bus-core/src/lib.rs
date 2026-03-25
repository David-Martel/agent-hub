//! Shared runtime for agent-bus: models, settings, storage, ops, token, output.

use std::sync::OnceLock;

pub mod channels;
pub mod codex_bridge;
pub mod journal;
pub mod models;
pub mod ops;
pub mod output;
pub mod postgres_store;
pub mod redis_bus;
pub mod settings;
pub mod token;
pub mod validation;

use postgres_store::PgWriter;

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
