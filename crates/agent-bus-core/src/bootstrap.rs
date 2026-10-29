use crate::error::Result;
use crate::postgres_store::{PgWriter, spawn_pg_health_monitor};
use crate::settings::Settings;

/// A guard that manages the lifetime of background tasks started during bootstrap.
///
/// Dropping the guard drains the [`PgWriter`] queue, so an early return (`?`),
/// a panic unwind or a normal exit all flush queued `PostgreSQL` writes. Before
/// agent-hub#194 only an explicit [`BootstrapGuard::shutdown`] drained, and any
/// error path that skipped it left messages in Redis that never reached
/// `PostgreSQL`.
#[derive(Debug)]
pub struct BootstrapGuard {
    writer: PgWriter,
    pg_handle: Option<tokio::task::JoinHandle<()>>,
}

impl BootstrapGuard {
    pub(crate) fn from_parts(writer: PgWriter, pg_handle: tokio::task::JoinHandle<()>) -> Self {
        Self {
            writer,
            pg_handle: Some(pg_handle),
        }
    }

    /// Wait for the background processes to shut down.
    pub fn shutdown(mut self) {
        self.drain();
    }

    /// Ask the writer to flush and stop, then wait for it. Idempotent.
    ///
    /// Waiting needs a multi-thread runtime context. On a current-thread
    /// runtime blocking would deadlock the writer task, and with no runtime
    /// there is nothing to block on, so the shutdown request is sent and the
    /// wait is skipped.
    fn drain(&mut self) {
        let Some(handle) = self.pg_handle.take() else {
            return;
        };
        let can_block = tokio::runtime::Handle::try_current().is_ok_and(|runtime| {
            runtime.runtime_flavor() != tokio::runtime::RuntimeFlavor::CurrentThread
        });
        if can_block {
            self.writer.shutdown_and_wait(handle);
        } else {
            self.writer.request_shutdown();
        }
    }
}

impl Drop for BootstrapGuard {
    fn drop(&mut self) {
        self.drain();
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
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .with_writer(std::io::stderr)
        .try_init(); // Ignore if it's already initialized by another module/test

    let settings = Settings::from_env();
    settings.validate().map_err(|e| {
        crate::error::AgentBusError::Internal(format!("Settings validation failed: {e}"))
    })?;

    let (writer, pg_handle) = PgWriter::spawn(settings.clone());
    let _ = crate::init_pg_writer(writer.clone());

    if settings.database_url.is_some() {
        spawn_pg_health_monitor(settings.clone(), 30);
    }

    Ok((settings, BootstrapGuard::from_parts(writer, pg_handle)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Message;

    fn message() -> Message {
        Message {
            id: uuid::Uuid::now_v7().to_string(),
            timestamp_utc: "2026-10-29T00:00:00.000000Z".to_owned(),
            protocol_version: "1.0".to_owned(),
            from: "test".to_owned(),
            to: "all".to_owned(),
            topic: "status".to_owned(),
            body: "agent-hub#194".to_owned(),
            thread_id: None,
            tags: vec![].into(),
            priority: "normal".to_owned(),
            request_ack: false,
            reply_to: None,
            metadata: serde_json::Value::Object(serde_json::Map::new()),
            stream_id: None,
            client_msg_id: None,
            origin_hub: None,
            origin_seq: None,
            hlc: None,
            sensitivity: None,
        }
    }

    /// Spawn a writer that has no database, so a settled write is instant.
    fn spawn_writer() -> (PgWriter, tokio::task::JoinHandle<()>) {
        let mut settings = Settings::for_test();
        settings.database_url = None;
        PgWriter::spawn(settings)
    }

    /// Protects: a queued PG write survives an early return from `run()`.
    /// Detects: dropping the guard without an explicit `shutdown()` (every
    /// `?` between the Redis write and the end of `run()`) left the queue
    /// unflushed, so the message stayed Redis-only (agent-hub#194).
    /// Needs: a multi-thread runtime, as in the three binaries.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_guard_flushes_queued_writes() {
        let (writer, handle) = spawn_writer();
        let guard = BootstrapGuard::from_parts(writer.clone(), handle);
        for _ in 0..5 {
            writer.send_message(&message());
        }
        assert_eq!(writer.pending(), 5, "precondition: writes are queued");
        drop(guard);
        assert_eq!(
            writer.pending(),
            0,
            "drop must drain the write-behind queue"
        );
    }

    /// Protects: explicit `shutdown()` still drains, and a second drain is inert.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn explicit_shutdown_flushes_queued_writes() {
        let (writer, handle) = spawn_writer();
        let guard = BootstrapGuard::from_parts(writer.clone(), handle);
        writer.send_message(&message());
        guard.shutdown();
        assert_eq!(writer.pending(), 0);
    }

    /// Protects: a closed writer reports the lost write instead of hiding it.
    /// Detects: `send_message` ignoring a closed channel (`let _ = tx.send`).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_after_shutdown_is_counted_as_dropped() {
        let (writer, handle) = spawn_writer();
        writer.shutdown_and_wait(handle);
        let before = crate::postgres_store::pg_metrics()
            .dropped_writes
            .load(std::sync::atomic::Ordering::Relaxed);
        writer.send_message(&message());
        let after = crate::postgres_store::pg_metrics()
            .dropped_writes
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            after > before,
            "a write to a dead writer must count as dropped"
        );
        assert_eq!(writer.pending(), 0);
    }

    /// Protects: the guard never panics or hangs on a current-thread runtime.
    #[tokio::test]
    async fn drop_on_current_thread_runtime_does_not_block() {
        let (writer, handle) = spawn_writer();
        let guard = BootstrapGuard::from_parts(writer, handle);
        drop(guard);
    }
}
