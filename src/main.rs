mod cli;
mod config;
mod config_commands;
mod control;
mod daemon;
mod daemon_client;
mod daemon_commands;
mod diagnostics;
mod jsonrpc;
mod local_security;
mod mcp_bridge;
mod mcp_cli;
mod mcp_client;
mod mcp_session;
mod oauth;
mod pool;
mod proxy;
mod request_deadline;
mod server_config;
mod socket_proxy;
mod tool_arguments;
mod tool_discovery;
mod tool_filter;
mod tool_output;
mod transport;
mod types;
mod upstream;
mod upstream_http;
mod upstream_process;
mod upstream_stdio;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run().await
}
