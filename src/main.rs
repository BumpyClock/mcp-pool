mod cli;
mod cli_progress;
mod config;
mod config_commands;
mod config_import;
mod config_import_auth;
mod config_import_sources;
mod config_paths;
mod configured_cli;
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
mod mcp_protocol;
mod mcp_session;
mod oauth;
mod pool;
mod proxy;
mod request_deadline;
mod server_config;
mod socket_proxy;
mod tool_arguments;
mod tool_discovery;
mod tool_documentation;
mod tool_filter;
mod tool_output;
mod transport;
mod types;
mod upstream;
mod upstream_http;
mod upstream_process;
mod upstream_stdio;

#[tokio::main]
async fn main() -> anyhow::Result<std::process::ExitCode> {
    match cli::run().await {
        Err(error) if error.is::<tool_discovery::QuietFailure>() => {
            Ok(std::process::ExitCode::FAILURE)
        }
        outcome => outcome.map(|()| std::process::ExitCode::SUCCESS),
    }
}
