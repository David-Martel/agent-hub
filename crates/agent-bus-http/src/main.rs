mod http;

use agent_bus_core::{bootstrap, maybe_announce_startup};
use anyhow::{Context, Result};

fn main() -> Result<()> {
    let handle = std::thread::Builder::new()
        .name("agent-bus-http-main".to_owned())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("failed to build Tokio runtime")?;
            runtime.block_on(run())
        })
        .context("failed to spawn agent-bus-http main thread")?;

    match handle.join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

async fn run() -> Result<()> {
    let (settings, guard) = bootstrap()?;
    maybe_announce_startup(&settings);
    http::start_http_server(settings, http::DEFAULT_PORT).await?;
    guard.shutdown();
    Ok(())
}
