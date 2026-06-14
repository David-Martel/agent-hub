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
    http::start_http_server(settings, resolve_port()).await?;
    guard.shutdown();
    Ok(())
}

/// Resolve the listen port: `--port N` / `--port=N` argv flag →
/// `AGENT_BUS_HTTP_PORT` env → [`http::DEFAULT_PORT`]. Previously the binary
/// ignored argv entirely and always bound `DEFAULT_PORT`, so
/// `agent-bus-http serve --port 9000` silently bound 8400 instead.
fn resolve_port() -> u16 {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--port" {
            if let Some(p) = args.next().and_then(|v| v.parse::<u16>().ok()) {
                return p;
            }
        } else if let Some(p) = arg
            .strip_prefix("--port=")
            .and_then(|v| v.parse::<u16>().ok())
        {
            return p;
        }
    }
    std::env::var("AGENT_BUS_HTTP_PORT")
        .ok()
        .and_then(|v| v.parse::<u16>().ok())
        .unwrap_or(http::DEFAULT_PORT)
}
