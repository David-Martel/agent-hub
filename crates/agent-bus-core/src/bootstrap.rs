use crate::postgres_store::{PgWriter, spawn_pg_health_monitor};
use crate::settings::Settings;
use crate::error::Result;

/// A guard that manages the lifetime of background tasks started during bootstrap.
/// When this guard is dropped or explicitly shutdown, it ensures clean termination
/// of components like the `PgWriter`.
#[derive(Debug)]
pub struct BootstrapGuard {
    pg_handle: tokio::task::JoinHandle<()>,
}

impl BootstrapGuard {
    /// Wait for the background processes to shut down.
    pub fn shutdown(self) {
        if let Some(writer) = crate::pg_writer() {
            writer.shutdown_and_wait(self.pg_handle);
        } else {
            self.pg_handle.abort();
        }
    }
}

/// Initialize the runtime tracing, load settings, and launch essential
/// background tasks like the `PgWriter` and `pg_health_monitor`.
///
/// Returns the validated `Settings` and a `BootstrapGuard` that must be
/// kept alive for the duration of the process.
///
/// # Errors
/// Returns an error if settings validation fails or tracing fails to initialize.
pub fn bootstrap() -> Result<(Settings, BootstrapGuard)> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .with_writer(std::io::stderr)
        .try_init()
        .ok(); // Ignore if it's already initialized by another module/test

    let settings = Settings::from_env();
    settings.validate().map_err(|_| crate::error::AgentBusError::Internal("Settings validation failed".to_string()))?;

    let (writer, pg_handle) = PgWriter::spawn(settings.clone());
    let _ = crate::init_pg_writer(writer);

    if settings.database_url.is_some() {
        spawn_pg_health_monitor(settings.clone(), 30);
    }

    Ok((settings, BootstrapGuard { pg_handle }))
}
