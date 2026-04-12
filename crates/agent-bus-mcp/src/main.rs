mod mcp;

use agent_bus_core::{bootstrap, maybe_announce_startup};
use anyhow::{Context, Result};
use mcp::AgentBusMcpServer;
use rmcp::serve_server;

fn main() -> Result<()> {
    let handle = std::thread::Builder::new()
        .name("agent-bus-mcp-main".to_owned())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to build Tokio runtime")?;
            runtime.block_on(run())
        })
        .context("failed to spawn agent-bus-mcp main thread")?;

    match handle.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

async fn run() -> Result<()> {
    let (settings, guard) = bootstrap()?;

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

    guard.shutdown();
    Ok(())
}
