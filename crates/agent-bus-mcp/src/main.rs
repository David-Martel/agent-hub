mod hub_transport;
mod mcp;

use agent_bus_core::{bootstrap, maybe_announce_startup};
use anyhow::{Context, Result};
use mcp::AgentBusMcpServer;
use rmcp::serve_server;

fn main() -> Result<()> {
    let handle = std::thread::Builder::new()
        .name("agent-bus-mcp-main".to_owned())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| run_with_mcp_runtime(run()))
        .context("failed to spawn agent-bus-mcp main thread")?;

    match handle.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

struct McpRuntimeOwner(Option<tokio::runtime::Runtime>);

impl Drop for McpRuntimeOwner {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            // A cancelled alias probe can leave OS DNS in the blocking pool.
            // Session completion must not wait for that remaining lookup.
            runtime.shutdown_background();
        }
    }
}

fn run_with_mcp_runtime(action: impl Future<Output = Result<()>>) -> Result<()> {
    let owner = McpRuntimeOwner(Some(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to build Tokio runtime")?,
    ));
    // block_on leaves its entered context before this owner drops, also when
    // the future unwinds. run() retains its existing explicit service cleanup.
    owner
        .0
        .as_ref()
        .context("MCP runtime unavailable")?
        .block_on(action)
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
