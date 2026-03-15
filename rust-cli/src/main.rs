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

mod cli;
mod commands;
mod http;
mod journal;
mod mcp;
mod models;
mod output;
mod postgres_store;
mod redis_bus;
mod settings;
mod validation;

use anyhow::{Context as _, Result};
use clap::Parser;
use mimalloc::MiMalloc;
use rmcp::serve_server;

use cli::{Cli, Cmd};
use commands::{
    PresenceArgs, ReadArgs, SendArgs, cmd_ack, cmd_export, cmd_health, cmd_journal, cmd_presence,
    cmd_presence_history, cmd_presence_list, cmd_prune, cmd_read, cmd_send, cmd_watch,
};
use http::start_http_server;
use mcp::AgentBusMcpServer;
use models::STARTUP_PRESENCE_TTL;
use postgres_store::probe_postgres;
use redis_bus::{bus_post_message, bus_set_presence, connect};
use settings::Settings;

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
    let _ = bus_set_presence(
        &mut conn,
        settings,
        &settings.service_agent_id,
        "online",
        None,
        &caps,
        STARTUP_PRESENCE_TTL,
        &meta,
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

        Cmd::Serve {
            ref transport,
            port,
        } => {
            if transport == "http" {
                maybe_announce_startup(&settings);
                start_http_server(settings, port).await?;
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
        }
    }

    Ok(())
}
