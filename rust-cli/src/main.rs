//! Fast standalone CLI + MCP server for agent-bus coordination.
//!
//! Direct Redis client — no Python interpreter, no GIL, instant startup.
//! Full feature parity with the Python agent-bus-mcp server, including MCP stdio mode.
//!
//! # Example
//!
//! ```bash
//! agent-bus health --encoding compact
//! agent-bus send --from-agent claude --to-agent codex --topic test --body "hello"
//! agent-bus read --agent codex --since-minutes 5 --encoding human
//! agent-bus serve --transport stdio
//! ```

// ---------------------------------------------------------------------------
// Lint suppressions at the crate level
// ---------------------------------------------------------------------------
#![expect(
    clippy::multiple_crate_versions,
    reason = "dependency diamond between rmcp and redis — not our choice"
)]

mod channels;
mod cli;
mod codex_bridge;
mod commands;
mod http;
mod journal;
mod mcp;
mod mcp_discovery;
mod models;
mod monitor;
mod output;
mod postgres_store;
mod redis_bus;
mod settings;
mod validation;

use std::sync::OnceLock;

use anyhow::{Context as _, Result};
use clap::Parser;
use mimalloc::MiMalloc;
use rmcp::serve_server;

use cli::{Cli, Cmd};
use commands::{
    PresenceArgs, ReadArgs, SendArgs, cmd_ack, cmd_batch_send, cmd_claim, cmd_claims,
    cmd_codex_sync, cmd_dedup, cmd_export, cmd_health, cmd_journal, cmd_monitor, cmd_pending_acks,
    cmd_post_direct, cmd_post_group, cmd_presence, cmd_presence_history, cmd_presence_list,
    cmd_prune, cmd_read, cmd_read_direct, cmd_read_group, cmd_resolve, cmd_send, cmd_session_summary,
    cmd_sync, cmd_watch,
};
use http::{start_http_server, start_mcp_http_server};
use mcp::AgentBusMcpServer;
use models::STARTUP_PRESENCE_TTL;
use postgres_store::{PgWriter, probe_postgres};
use redis_bus::{bus_post_message, bus_set_presence, connect};
use settings::Settings;

// ---------------------------------------------------------------------------
// Section 8 – Global async PG writer (Task 3)
// ---------------------------------------------------------------------------

/// Process-lifetime [`PgWriter`] handle, initialized once before any transport
/// starts.  Callers retrieve it via [`pg_writer()`].
static PG_WRITER: OnceLock<PgWriter> = OnceLock::new();

/// Returns a reference to the global [`PgWriter`], or `None` before init.
pub(crate) fn pg_writer() -> Option<&'static PgWriter> {
    PG_WRITER.get()
}

/// Use mimalloc for notable allocation performance gains (M-MIMALLOC-APPS).
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// ---------------------------------------------------------------------------
// Section 9 – Startup announcement helper
// ---------------------------------------------------------------------------

fn maybe_announce_startup(settings: &Settings) {
    if !settings.startup_enabled {
        return;
    }
    let Ok(mut conn) = connect(settings) else {
        return; // silently skip if Redis not up yet
    };
    let meta = serde_json::json!({"service": "agent-bus", "startup": true});
    let mut caps = vec!["mcp".to_owned(), "redis".to_owned()];
    if probe_postgres(settings).0 == Some(true) {
        caps.push("postgres".to_owned());
    }
    let writer = pg_writer();
    let _ = bus_set_presence(
        &mut conn,
        settings,
        &settings.service_agent_id,
        "online",
        None,
        &caps,
        STARTUP_PRESENCE_TTL,
        &meta,
        writer,
    );
    let _ = bus_post_message(
        &mut conn,
        settings,
        &settings.service_agent_id,
        &settings.startup_recipient,
        &settings.startup_topic,
        &settings.startup_body,
        None,
        &[
            "startup".to_owned(),
            "system".to_owned(),
            "health".to_owned(),
        ],
        "normal",
        false,
        None,
        &meta,
        writer,
        false, // startup broadcast: no SSE clients yet
    );
}

// ---------------------------------------------------------------------------
// Section 10 – main
// ---------------------------------------------------------------------------

#[expect(
    clippy::too_many_lines,
    reason = "main command dispatch — extracting further would obscure flow"
)]
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let settings = Settings::from_env();
    settings.validate()?;

    // Initialize the global PgWriter before any transport starts.
    // When database_url is absent the writer is still created but all writes
    // are silently discarded by the background task.
    let (writer, pg_handle) = PgWriter::spawn(settings.clone());
    let _ = PG_WRITER.set(writer);

    // Proactively reset the circuit breaker every 30 s so that a transient PG
    // outage does not permanently suppress writes for the lifetime of the process.
    if settings.database_url.is_some() {
        crate::postgres_store::spawn_pg_health_monitor(settings.clone(), 30);
    }

    let cli = Cli::parse();

    match cli.command {
        Cmd::Health { ref encoding } => {
            cmd_health(&settings, encoding);
        }

        Cmd::Send {
            ref from_agent,
            ref to_agent,
            ref topic,
            ref body,
            ref thread_id,
            ref tag,
            ref priority,
            request_ack,
            ref reply_to,
            ref metadata,
            ref schema,
            ref encoding,
        } => {
            cmd_send(
                &settings,
                &SendArgs {
                    from_agent,
                    to_agent,
                    topic,
                    body,
                    thread_id,
                    tags: tag,
                    priority,
                    request_ack,
                    reply_to,
                    metadata: metadata.as_deref(),
                    schema: schema.as_deref(),
                    encoding,
                },
            )?;
        }

        Cmd::Read {
            ref agent,
            ref from_agent,
            since_minutes,
            limit,
            exclude_broadcast,
            ref encoding,
        } => {
            cmd_read(
                &settings,
                &ReadArgs {
                    agent,
                    from_agent,
                    since_minutes,
                    limit,
                    exclude_broadcast,
                    encoding,
                },
            )?;
        }

        Cmd::Watch {
            ref agent,
            history,
            exclude_broadcast,
            ref encoding,
        } => {
            cmd_watch(&settings, agent, history, exclude_broadcast, encoding)?;
        }

        Cmd::Ack {
            ref agent,
            ref message_id,
            ref body,
            ref encoding,
        } => {
            cmd_ack(&settings, agent, message_id, body, encoding)?;
        }

        Cmd::Presence {
            ref agent,
            ref status,
            ref session_id,
            ref capability,
            ttl_seconds,
            ref metadata,
            ref encoding,
        } => {
            cmd_presence(
                &settings,
                &PresenceArgs {
                    agent,
                    status,
                    session_id,
                    capabilities: capability,
                    ttl_seconds,
                    metadata: metadata.as_deref(),
                    encoding,
                },
            )?;
        }

        Cmd::PresenceList { ref encoding } => {
            cmd_presence_list(&settings, encoding)?;
        }

        Cmd::Prune {
            older_than_days,
            ref encoding,
        } => {
            cmd_prune(&settings, older_than_days, encoding)?;
        }

        Cmd::Export {
            ref agent,
            ref from_agent,
            since_minutes,
            limit,
        } => {
            cmd_export(
                &settings,
                agent.as_deref(),
                from_agent.as_deref(),
                since_minutes,
                limit,
            )?;
        }

        Cmd::PresenceHistory {
            ref agent,
            since_minutes,
            limit,
            ref encoding,
        } => {
            cmd_presence_history(&settings, agent.as_deref(), since_minutes, limit, encoding)?;
        }

        Cmd::Journal {
            ref tag,
            ref from_agent,
            since_minutes,
            limit,
            ref output,
        } => {
            cmd_journal(
                &settings,
                tag.as_deref(),
                from_agent.as_deref(),
                since_minutes,
                limit,
                output,
            )?;
        }

        Cmd::PendingAcks {
            ref agent,
            ref encoding,
        } => {
            cmd_pending_acks(&settings, agent.as_deref(), encoding)?;
        }

        Cmd::Sync {
            limit,
            ref encoding,
        } => {
            cmd_sync(&settings, limit, encoding)?;
        }

        Cmd::CodexSync {
            limit,
            ref encoding,
        } => {
            cmd_codex_sync(&settings, limit, encoding)?;
        }

        Cmd::Monitor {
            ref session,
            refresh,
        } => {
            cmd_monitor(&settings, session.as_deref(), refresh)?;
        }

        Cmd::Serve {
            ref transport,
            port,
        } => {
            if transport == "http" {
                maybe_announce_startup(&settings);
                start_http_server(settings, port).await?;
            } else if transport == "mcp-http" {
                // MCP Streamable HTTP transport (spec 2025-06-18).
                // Announces startup then starts the Axum server on /mcp.
                maybe_announce_startup(&settings);
                start_mcp_http_server(settings, port).await?;
            } else {
                // Default: MCP stdio transport.
                // Start the MCP server FIRST so it can respond to initialize
                // immediately, then announce startup in the background.
                let announce_settings = settings.clone();
                let server = AgentBusMcpServer::new(settings);
                let mcp_transport = (tokio::io::stdin(), tokio::io::stdout());
                let service = serve_server(server, mcp_transport)
                    .await
                    .context("MCP server init failed")?;
                // Announce after MCP handshake completes (non-blocking)
                tokio::spawn(async move {
                    maybe_announce_startup(&announce_settings);
                });
                service.waiting().await.context("MCP server loop error")?;
            }
            // Server commands run indefinitely — pg_handle is cancelled when
            // the process terminates; no explicit shutdown needed.
            pg_handle.abort();
            return Ok(());
        }

        Cmd::BatchSend {
            ref file,
            ref encoding,
        } => {
            cmd_batch_send(&settings, file, encoding)?;
        }

        // --- Channel commands ------------------------------------------------
        Cmd::PostDirect {
            ref from_agent,
            ref to_agent,
            ref topic,
            ref body,
            ref thread_id,
            ref tag,
            ref encoding,
        } => {
            cmd_post_direct(
                &settings,
                from_agent,
                to_agent,
                topic,
                body,
                thread_id.as_deref(),
                tag,
                encoding,
            )?;
        }

        Cmd::ReadDirect {
            ref agent_a,
            ref agent_b,
            limit,
            ref encoding,
        } => {
            cmd_read_direct(&settings, agent_a, agent_b, limit, encoding)?;
        }

        Cmd::PostGroup {
            ref group,
            ref from_agent,
            ref topic,
            ref body,
            ref thread_id,
            ref encoding,
        } => {
            cmd_post_group(
                &settings,
                group,
                from_agent,
                topic,
                body,
                thread_id.as_deref(),
                encoding,
            )?;
        }

        Cmd::ReadGroup {
            ref group,
            limit,
            ref encoding,
        } => {
            cmd_read_group(&settings, group, limit, encoding)?;
        }

        Cmd::Claim {
            ref resource,
            ref agent,
            ref reason,
            ref encoding,
        } => {
            cmd_claim(&settings, resource, agent, reason, encoding)?;
        }

        Cmd::Claims {
            ref resource,
            ref status,
            ref encoding,
        } => {
            cmd_claims(&settings, resource.as_deref(), status.as_deref(), encoding)?;
        }

        Cmd::Resolve {
            ref resource,
            ref winner,
            ref reason,
            ref resolved_by,
            ref encoding,
        } => {
            cmd_resolve(&settings, resource, winner, reason, resolved_by, encoding)?;
        }

        // --- Session management commands (Task 4.2 / 4.3) -------------------
        Cmd::SessionSummary {
            ref session,
            ref encoding,
        } => {
            cmd_session_summary(&settings, session, encoding)?;
        }

        Cmd::Dedup {
            ref session,
            ref agent,
            since_minutes,
            ref encoding,
        } => {
            cmd_dedup(
                &settings,
                session.as_deref(),
                agent.as_deref(),
                since_minutes,
                encoding,
            )?;
        }
    }

    // Flush any enqueued PG writes before the process exits.  This is only
    // reached by non-server CLI commands; server commands return early above.
    if let Some(writer) = pg_writer() {
        writer.shutdown_and_wait(pg_handle);
    } else {
        pg_handle.abort();
    }

    Ok(())
}
