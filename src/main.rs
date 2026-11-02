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
mod terminal_color;
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

fn main() -> anyhow::Result<std::process::ExitCode> {
    let arguments = std::env::args_os()
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| anyhow::anyhow!("command-line arguments must be valid Unicode"))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let mut runtime = tokio::runtime::Builder::new_multi_thread();
    runtime.enable_all();
    if let Some(workers) = cli_worker_threads(
        arguments.get(1..).unwrap_or_default(),
        std::env::var_os("TOKIO_WORKER_THREADS").is_some(),
    ) {
        runtime.worker_threads(workers);
    }
    match runtime.build()?.block_on(cli::run(arguments)) {
        Err(error) if error.is::<tool_discovery::QuietFailure>() => {
            Ok(std::process::ExitCode::FAILURE)
        }
        outcome => outcome.map(|()| std::process::ExitCode::SUCCESS),
    }
}

fn cli_worker_threads(arguments: &[String], explicitly_configured: bool) -> Option<usize> {
    if explicitly_configured {
        return None;
    }
    let command = mcp_cli::command_position(arguments)
        .and_then(|position| arguments.get(position))
        .map(String::as_str);
    match command {
        Some("serve" | "proxy" | "pool" | "daemon") => None,
        _ => Some(1),
    }
}

#[cfg(test)]
mod runtime_tests {
    use super::*;

    #[test]
    fn short_lived_commands_do_not_start_a_worker_per_host_cpu() {
        for arguments in [
            vec!["call", "docs.search"],
            vec!["--config", "serve", "list"],
            vec!["--root=pool", "docs.search"],
            vec!["--version"],
        ] {
            let arguments = arguments.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(cli_worker_threads(&arguments, false), Some(1));
        }
    }

    #[test]
    fn long_lived_commands_and_explicit_worker_settings_keep_tokio_defaults() {
        for arguments in [
            vec!["pool", "serve"],
            vec!["serve", "--stdio"],
            vec!["--config", "config.json", "proxy", "docs"],
            vec!["daemon", "start", "--foreground"],
        ] {
            let arguments = arguments.into_iter().map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(cli_worker_threads(&arguments, false), None);
        }
        assert_eq!(cli_worker_threads(&["call".into()], true), None);
    }
}
