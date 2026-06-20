// mcp-pool: standalone MCP server pool CLI.
// Modules are declared here (single binary crate, no mod.rs / lib.rs).

mod cli;
mod config;
mod control;
mod daemon;
mod diagnostics;
mod jsonrpc;
mod pool;
mod proxy;
mod socket_proxy;
mod transport;
mod types;
mod upstream;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run().await
}
