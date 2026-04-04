//! Fast standalone CLI + MCP server for agent-bus coordination.
//!
//! Direct Redis client with CLI, HTTP, and MCP transports.

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
mod ops;
mod output;
mod postgres_store;
mod redis_bus;
mod server_mode;
mod settings;
mod token;
mod validation;

use std::{ffi::OsString, iter};

use anyhow::{Context as _, Result};
use clap::Parser;
use mimalloc::MiMalloc;
use rmcp::serve_server;

use agent_bus_core::{init_pg_writer, maybe_announce_startup, pg_writer};
use cli::{Cli, Cmd};
use commands::{
    CompactContextArgs, PresenceArgs, ReadArgs, SendArgs, cmd_ack, cmd_batch_send, cmd_claim,
    cmd_claims, cmd_codex_sync, cmd_compact_context, cmd_dedup, cmd_export, cmd_health,
    cmd_inventory, cmd_journal, cmd_knock, cmd_monitor, cmd_peek_tasks, cmd_pending_acks,
    cmd_post_direct, cmd_post_group, cmd_presence, cmd_presence_history, cmd_presence_list,
    cmd_prune, cmd_pull_task, cmd_push_task, cmd_read, cmd_read_direct, cmd_read_group,
    cmd_release_claim, cmd_renew_claim, cmd_resolve, cmd_send, cmd_service, cmd_session_summary,
    cmd_sync, cmd_token_count, cmd_watch,
};
use http::{start_http_server, start_mcp_http_server};
use mcp::AgentBusMcpServer;
use postgres_store::PgWriter;
use settings::Settings;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const MAIN_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;

/// Run the full compatibility CLI entrypoint.
///
/// # Errors
///
/// Returns an error if argument parsing, runtime startup, or command execution fails.
pub fn main_entry() -> Result<()> {
    main_entry_with_args(std::env::args_os())
}

/// Run the HTTP-focused binary entrypoint.
///
/// When invoked without subcommands, this defaults to `serve --transport http`.
///
/// # Errors
///
/// Returns an error if argument parsing, runtime startup, or command execution fails.
pub fn http_entry() -> Result<()> {
    main_entry_with_default_args(["serve", "--transport", "http"])
}

/// Run the MCP-focused binary entrypoint.
///
/// When invoked without subcommands, this defaults to `serve --transport stdio`.
///
/// # Errors
///
/// Returns an error if argument parsing, runtime startup, or command execution fails.
pub fn mcp_entry() -> Result<()> {
    main_entry_with_default_args(["serve", "--transport", "stdio"])
}

fn main_entry_with_default_args<const N: usize>(default_args: [&str; N]) -> Result<()> {
    main_entry_with_args(args_with_optional_default(
        std::env::args_os().collect(),
        default_args,
    ))
}

fn args_with_optional_default<const N: usize>(
    current_args: Vec<OsString>,
    default_args: [&str; N],
) -> Vec<OsString> {
    if current_args.len() > 1 {
        return current_args;
    }

    let argv0 = current_args
        .first()
        .cloned()
        .unwrap_or_else(|| OsString::from("agent-bus"));
    iter::once(argv0)
        .chain(default_args.into_iter().map(OsString::from))
        .collect()
}

fn main_entry_with_args(args: impl IntoIterator<Item = OsString>) -> Result<()> {
    let args = args.into_iter().collect::<Vec<_>>();
    let handle = std::thread::Builder::new()
        .name("agent-bus-main".to_owned())
        .stack_size(MAIN_THREAD_STACK_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to build Tokio runtime")?;
            runtime.block_on(run(args))
        })
        .context("failed to spawn agent-bus main thread")?;

    match handle.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "main command dispatch — extracting further would obscure flow"
)]
async fn run(args: Vec<OsString>) -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let settings = Settings::from_env();
    settings.validate()?;

    let (writer, pg_handle) = PgWriter::spawn(settings.clone());
    let _ = init_pg_writer(writer);

    if settings.database_url.is_some() {
        crate::postgres_store::spawn_pg_health_monitor(settings.clone(), 30);
    }

    let cli = Cli::parse_from(args);
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
            ref repo,
            ref session,
            ref tag,
            ref thread_id,
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
                    repo,
                    session,
                    tags: tag,
                    thread_id,
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
            ref repo,
            ref session,
            ref tag,
            ref thread_id,
            since_minutes,
            limit,
        } => {
            cmd_export(
                &settings,
                agent.as_deref(),
                from_agent.as_deref(),
                repo,
                session,
                tag,
                thread_id,
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
            ref repo,
            ref session,
            ref tag,
            ref thread_id,
            ref from_agent,
            since_minutes,
            limit,
            ref output,
        } => {
            cmd_journal(
                &settings,
                repo,
                session,
                tag,
                thread_id,
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
                maybe_announce_startup(&settings);
                start_mcp_http_server(settings, port).await?;
            } else {
                let announce_settings = settings.clone();
                let server = AgentBusMcpServer::new(settings);
                let mcp_transport = (tokio::io::stdin(), tokio::io::stdout());
                let service = serve_server(server, mcp_transport)
                    .await
                    .context("MCP server init failed")?;
                tokio::spawn(async move {
                    maybe_announce_startup(&announce_settings);
                });
                service.waiting().await.context("MCP server loop error")?;
            }
        }

        Cmd::Service {
            ref action,
            ref reason,
            ref base_url,
            ref service_name,
            timeout_seconds,
            ref encoding,
        } => {
            cmd_service(
                &settings,
                action,
                reason.as_deref(),
                base_url.as_deref(),
                service_name.as_deref(),
                timeout_seconds,
                encoding,
            )?;
        }

        Cmd::BatchSend {
            ref file,
            ref encoding,
        } => {
            cmd_batch_send(&settings, file, encoding)?;
        }

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
            ref mode,
            ref namespace,
            ref scope_kind,
            ref scope_path,
            ref repo_scope,
            ref thread_id,
            lease_ttl_seconds,
            ref encoding,
        } => {
            cmd_claim(
                &settings,
                resource,
                agent,
                reason,
                mode,
                namespace.as_deref(),
                scope_kind.as_deref(),
                scope_path.as_deref(),
                repo_scope,
                thread_id.as_deref(),
                lease_ttl_seconds,
                encoding,
            )?;
        }

        Cmd::RenewClaim {
            ref resource,
            ref agent,
            lease_ttl_seconds,
            ref encoding,
        } => {
            cmd_renew_claim(&settings, resource, agent, lease_ttl_seconds, encoding)?;
        }

        Cmd::ReleaseClaim {
            ref resource,
            ref agent,
            ref encoding,
        } => {
            cmd_release_claim(&settings, resource, agent, encoding)?;
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

        Cmd::Knock {
            ref from_agent,
            ref to_agent,
            ref body,
            ref thread_id,
            ref tag,
            request_ack,
            ref encoding,
        } => {
            cmd_knock(
                &settings,
                from_agent,
                to_agent,
                body,
                thread_id.as_deref(),
                tag,
                request_ack,
                encoding,
            )?;
        }

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

        Cmd::TokenCount {
            ref text,
            ref encoding,
        } => {
            cmd_token_count(text.as_deref(), encoding)?;
        }

        Cmd::CompactContext {
            ref agent,
            ref repo,
            ref session,
            ref tag,
            ref thread_id,
            since_minutes,
            max_tokens,
            ref encoding,
        } => {
            let args = CompactContextArgs {
                agent: agent.as_deref(),
                repo,
                session,
                tags: tag,
                thread_id,
                since_minutes,
                max_tokens,
                encoding,
            };
            cmd_compact_context(&settings, &args)?;
        }

        Cmd::PushTask {
            ref agent,
            ref task,
            ref encoding,
            ref repo,
            ref priority,
            ref tags,
            ref depends_on,
            ref reply_to,
            ref created_by,
        } => {
            cmd_push_task(
                &settings,
                agent,
                task,
                encoding,
                repo.as_deref(),
                priority,
                tags,
                depends_on,
                reply_to.as_deref(),
                created_by,
            )?;
        }

        Cmd::PullTask {
            ref agent,
            ref encoding,
        } => {
            cmd_pull_task(&settings, agent, encoding)?;
        }

        Cmd::PeekTasks {
            ref agent,
            limit,
            ref encoding,
        } => {
            cmd_peek_tasks(&settings, agent, limit, encoding)?;
        }

        Cmd::Inventory {
            ref repo,
            ref encoding,
        } => {
            cmd_inventory(&settings, repo.as_deref(), encoding)?;
        }
    }

    if let Some(writer) = pg_writer() {
        writer.shutdown_and_wait(pg_handle);
    } else {
        pg_handle.abort();
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::args_with_optional_default;
    use std::ffi::OsString;

    #[test]
    fn default_args_are_injected_when_only_argv0_is_present() {
        let args = args_with_optional_default(
            vec![OsString::from("agent-bus-http")],
            ["serve", "--transport", "http"],
        );
        let rendered = args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(rendered, ["agent-bus-http", "serve", "--transport", "http"]);
    }

    #[test]
    fn explicit_args_are_preserved_for_dedicated_bins() {
        let args = args_with_optional_default(
            vec![
                OsString::from("agent-bus-http"),
                OsString::from("--version"),
            ],
            ["serve", "--transport", "http"],
        );
        let rendered = args
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(rendered, ["agent-bus-http", "--version"]);
    }
}
