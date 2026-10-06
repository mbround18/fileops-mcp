//! stdio entry point for the fileops MCP server.

mod server;

use anyhow::Result;
use clap::Parser;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

/// Token-efficient file reading over the Model Context Protocol.
///
/// Speaks MCP on stdin/stdout, so it is normally launched by an MCP client rather than run
/// by hand. Register it with:
/// `claude mcp add --scope user fileops fileops-mcp`
#[derive(Debug, Parser)]
#[command(name = "fileops-mcp", version)]
struct Cli {
    /// Log filter for diagnostics on stderr, in `tracing` syntax (e.g. `debug`).
    #[arg(long, env = "FILEOPS_MCP_LOG", default_value = "info")]
    log: String,

    /// Default ceiling, in bytes, on a single response's rendered text. A request may
    /// raise or lower it; it cannot remove it.
    #[arg(long, env = "FILEOPS_MCP_MAX_BYTES")]
    max_bytes: Option<usize>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // stdout carries the protocol; diagnostics go to stderr only.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_new(&cli.log).unwrap_or_else(|_| EnvFilter::new("info")))
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    tracing::info!(version = env!("CARGO_PKG_VERSION"), "starting fileops-mcp");
    let service = server::FileOpsServer::new(cli.max_bytes)
        .serve(stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}
