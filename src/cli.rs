use std::io::{self, IsTerminal};

use clap::{FromArgMatches, Parser, Subcommand};

use crate::config;
use crate::configured_cli::Operation;
use crate::diagnostics;

#[path = "cli_native.rs"]
mod native;
#[path = "cli_output.rs"]
mod output;
#[path = "cli_routing.rs"]
mod routing;
#[cfg(test)]
use native::normalize_transport;
use native::{add_server, build_server_def, remove_server};
use output::print_response_data;

#[derive(Parser)]
#[command(
    name = "mcp-pool",
    version = env!("MCP_POOL_BUILD_VERSION"),
    about = "Pool MCP servers — one upstream, many clients",
    after_help = "MCP commands:\n  list, describe, list-tools  Discover tools\n  resource, resources        Read or discover resources\n  auth                       Authorize a configured server\n  vault                      Manage cached credentials\n  config                     Manage merged MCP configuration\n  import                     Copy external MCP definitions\n  daemon                     Manage the MCP pool daemon\n  serve                      Expose pooled tools over stdio or HTTP\n\nNative commands:\n  pool list|serve             Inspect or run the native pool daemon\n  start|stop|restart|status   Control pooled servers\n  add|remove|proxy|shutdown   Manage native pool entries"
)]
pub struct Cli {
    /// Read only this MCP config instead of merging home and project configs.
    #[arg(long, global = true)]
    pub config: Option<std::path::PathBuf>,

    /// Project directory for project config and credential selection.
    #[arg(long, global = true)]
    pub root: Option<std::path::PathBuf>,
    /// Enable diagnostic logging (also via MCP_POOL_DEBUG=1).
    #[arg(long, global = true)]
    pub debug: bool,

    /// Machine-readable JSON output (status / list).
    #[arg(long, global = true)]
    pub json: bool,

    /// Native status/pool-list rows; suppress progress for MCP commands.
    #[arg(long, global = true)]
    pub plain: bool,

    /// Disable colored output (also disabled when NO_COLOR is set or TERM=dumb).
    #[arg(long, global = true)]
    pub no_color: bool,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Call an MCP tool.
    Call {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    /// Read or discover MCP resources.
    Resource {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    /// Authorize a configured MCP server.
    Auth {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    /// Manage cached MCP credentials.
    Vault {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    /// Manage merged MCP configuration.
    Config {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    /// Manage the MCP pool daemon.
    Daemon {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    /// Copy external MCP server definitions into .mcp/config.json.
    Import {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        arguments: Vec<String>,
    },
    /// Run the native MCP pool daemon in the foreground.
    Serve,

    /// Start pooled MCP server(s): a single one by name, or all configured
    /// servers (started concurrently) when no name is given.
    Start { name: Option<String> },

    /// Stop a pooled MCP server.
    Stop { name: String },

    /// Restart a pooled MCP server.
    Restart { name: String },

    /// Show pool status (all servers, or one by name).
    Status { name: Option<String> },

    /// List configured servers without connecting.
    List,

    /// Add a server to config.
    /// Stdio: `add NAME -- COMMAND [ARGS...]`.
    /// Remote: `add NAME --url URL [--transport http|sse]`.
    Add {
        name: String,
        /// Remote URL (mutually exclusive with a stdio command).
        #[arg(long)]
        url: Option<String>,
        /// Remote transport: "http" or "sse" (default "http").
        #[arg(long)]
        transport: Option<String>,
        /// Optional explicit stdio command (alternative to trailing args).
        #[arg(long)]
        command: Option<String>,
        /// Print the planned config without writing.
        #[arg(long)]
        dry_run: bool,
        /// Trailing command + args (after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        trailing: Vec<String>,
    },

    /// Remove a server from config.
    Remove {
        name: String,
        /// Skip the interactive confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Bridge an agent's stdio to a pool socket (put this in the agent's MCP config).
    /// Never writes to stdout: stdout is the raw MCP byte stream.
    Proxy { name: String },

    /// Stop the daemon and all pooled servers.
    Shutdown,
}

pub async fn run() -> anyhow::Result<()> {
    let mut arguments: Vec<String> = std::env::args_os()
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| anyhow::anyhow!("command-line arguments must be valid Unicode"))
        })
        .collect::<anyhow::Result<_>>()?;
    let initial_command_position =
        crate::mcp_cli::command_position(arguments.get(1..).unwrap_or_default())
            .map(|position| position + 1);
    if let Some(position) = initial_command_position
        && arguments
            .get(position)
            .is_some_and(|argument| argument == "help")
    {
        arguments = routing::rewrite_help(arguments, position);
    }
    let command_position = crate::mcp_cli::command_position(arguments.get(1..).unwrap_or_default())
        .map(|position| position + 1);
    let native_namespace = command_position
        .and_then(|position| arguments.get(position))
        .is_some_and(|argument| argument == "pool");
    let native_command = command_position.and_then(|position| {
        let tail = arguments.get(position + 1..)?;
        crate::mcp_cli::command_position(tail).and_then(|index| tail.get(index))
    });
    if native_namespace && native_command.is_some_and(|command| routing::top_level_only(command)) {
        anyhow::bail!("MCP commands are top-level commands, not native pool commands");
    }
    if native_namespace
        && command_position
            .and_then(|position| arguments.get(position + 1))
            .is_some_and(|argument| argument == "list")
        && arguments
            .iter()
            .skip(command_position.unwrap_or_default() + 2)
            .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
    {
        println!(
            "Usage: mcp-pool pool list [--json|--plain]\nList configured native pool entries without connecting to MCP servers."
        );
        return Ok(());
    }
    if !native_namespace && crate::mcp_cli::handles(arguments.get(1..).unwrap_or_default()) {
        diagnostics::init_from_env();
        return crate::mcp_cli::run(arguments.into_iter().skip(1).collect()).await;
    }
    if !native_namespace
        && let Some(position) = command_position
        && arguments.get(position).is_some_and(|argument| {
            !argument.starts_with('-') && !routing::is_native_command(argument)
        })
    {
        let (config_path, root) = routing::global_paths(arguments.get(1..).unwrap_or_default())?;
        let routed = routing::infer(
            arguments.get(1..).unwrap_or_default(),
            position - 1,
            config_path,
            root.as_deref(),
        )?;
        diagnostics::init_from_env();
        return crate::mcp_cli::run(routed).await;
    }
    let native_arguments = if native_namespace {
        arguments
            .iter()
            .enumerate()
            .filter(|(index, _)| Some(*index) != command_position)
            .map(|(_, argument)| argument.clone())
            .collect()
    } else {
        arguments
    };
    let matches = routing::parser(native_namespace).get_matches_from(native_arguments);
    let cli = Cli::from_arg_matches(&matches)?;

    diagnostics::init_from_env();
    if cli.debug {
        diagnostics::set_enabled(true);
    }

    let mode = output_mode(&cli);
    let color = use_color(&cli);

    match cli.cmd {
        Cmd::Call { arguments } => run_mcp_subcommand("call", arguments).await,
        Cmd::Resource { arguments } => run_mcp_subcommand("resource", arguments).await,
        Cmd::Auth { arguments } => run_mcp_subcommand("auth", arguments).await,
        Cmd::Vault { arguments } => run_mcp_subcommand("vault", arguments).await,
        Cmd::Config { arguments } => run_mcp_subcommand("config", arguments).await,
        Cmd::Daemon { arguments } => run_mcp_subcommand("daemon", arguments).await,
        Cmd::Import { arguments } => {
            crate::config_import::run(cli.config, cli.root.as_deref(), arguments).await
        }
        Cmd::Serve => crate::daemon::serve().await,
        Cmd::Proxy { name } => {
            let configuration = crate::server_config::load_at(cli.config, cli.root.as_deref())?;
            let server = configuration
                .servers
                .get(&name)
                .ok_or_else(|| anyhow::anyhow!("unknown server: {name}"))?;
            let definition = crate::oauth::prepare(server, false).await?;
            let pool_name = crate::mcp_cli::pool_name(server, &definition)?;
            crate::proxy::run_resolved(&pool_name, &definition).await
        }
        Cmd::Add {
            name,
            url,
            transport,
            command,
            dry_run,
            trailing,
        } => {
            let definition = build_server_def(url, transport, command, trailing)?;
            add_server(&name, definition, dry_run, cli.config, cli.root.as_deref()).await
        }
        Cmd::Remove { name, yes } => {
            remove_server(&name, yes, cli.config, cli.root.as_deref()).await
        }
        Cmd::List => list_servers(mode, cli.config, cli.root.as_deref()),
        Cmd::Start { name } => {
            let request = match name {
                Some(name) => Operation::Start { name },
                None => Operation::StartAll,
            };
            control_round_trip(request, mode, color, cli.config, cli.root.as_deref()).await
        }
        Cmd::Stop { name } => {
            control_round_trip(
                Operation::Stop { name },
                mode,
                color,
                cli.config,
                cli.root.as_deref(),
            )
            .await
        }
        Cmd::Restart { name } => {
            control_round_trip(
                Operation::Restart { name },
                mode,
                color,
                cli.config,
                cli.root.as_deref(),
            )
            .await
        }
        Cmd::Status { name } => {
            control_round_trip(
                Operation::Status { name },
                mode,
                color,
                cli.config,
                cli.root.as_deref(),
            )
            .await
        }
        Cmd::Shutdown => {
            control_round_trip(
                Operation::Shutdown,
                mode,
                color,
                cli.config,
                cli.root.as_deref(),
            )
            .await
        }
    }
}

async fn run_mcp_subcommand(command: &str, arguments: Vec<String>) -> anyhow::Result<()> {
    let mut routed = Vec::with_capacity(arguments.len() + 1);
    routed.push(command.to_owned());
    routed.extend(arguments);
    crate::mcp_cli::run(routed).await
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    Json,
    Plain,
    Table,
}

fn output_mode(cli: &Cli) -> OutputMode {
    if cli.json {
        OutputMode::Json
    } else if cli.plain || !io::stdout().is_terminal() {
        OutputMode::Plain
    } else {
        OutputMode::Table
    }
}

fn use_color(cli: &Cli) -> bool {
    !cli.no_color
        && std::env::var_os("NO_COLOR").is_none()
        && !matches!(std::env::var("TERM").as_deref(), Ok("dumb"))
        && io::stdout().is_terminal()
}

/// Plain output is `name<TAB>status<TAB>transport<TAB>socket`; local config emits `-` for status.
fn list_servers(
    mode: OutputMode,
    path: Option<std::path::PathBuf>,
    root: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let configuration = crate::server_config::load_at(path, root)?;
    let entries: Vec<(String, String, String)> = configuration
        .servers
        .iter()
        .map(|(name, server)| {
            Ok((
                name.clone(),
                server.definition.transport_kind().to_string(),
                config::server_socket_path(&crate::mcp_cli::pool_name(
                    server,
                    &crate::oauth::identity_definition(server)?,
                )?)
                .to_string_lossy()
                .to_string(),
            ))
        })
        .collect::<anyhow::Result<_>>()?;

    if entries.is_empty() {
        if mode == OutputMode::Json {
            println!("[]")
        } else {
            println!("no servers configured")
        }
        return Ok(());
    }

    match mode {
        OutputMode::Json => {
            let value: Vec<serde_json::Value> = entries
                .iter()
                .map(|(name, transport, socket)| {
                    serde_json::json!({ "name": name, "transport": transport, "socket_path": socket })
                })
                .collect();
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        OutputMode::Plain => {
            for (name, transport, socket) in &entries {
                println!("{name}\t-\t{transport}\t{socket}");
            }
        }
        OutputMode::Table => {
            println!("{:<20} {:<10} SOCKET", "NAME", "TRANSPORT");
            for (name, transport, socket) in &entries {
                println!("{:<20} {:<10} {socket}", name, transport);
            }
        }
    }
    Ok(())
}

async fn control_round_trip(
    request: Operation,
    mode: OutputMode,
    color: bool,
    path: Option<std::path::PathBuf>,
    root: Option<&std::path::Path>,
) -> anyhow::Result<()> {
    let message = match &request {
        Operation::Start { .. } | Operation::StartAll => "Starting pooled MCP servers",
        Operation::Stop { .. } => "Stopping pooled MCP server",
        Operation::Restart { .. } => "Restarting pooled MCP server",
        Operation::Status { .. } => "Checking MCP pool status",
        Operation::Shutdown => "Stopping MCP pool daemon",
    };
    let mut progress = crate::cli_progress::Progress::new(mode == OutputMode::Table);
    let response = progress
        .wait(
            message,
            crate::configured_cli::control(&request, path, root),
        )
        .await?;
    progress.finish();

    if !response.ok {
        let message = response
            .error
            .unwrap_or_else(|| "unknown error".to_string());
        eprintln!("mcp-pool: {message}");
        std::process::exit(1);
    }

    let failed = matches!(request, Operation::StartAll)
        && response
            .data
            .as_ref()
            .and_then(|data| data.get("servers"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|servers| {
                servers.iter().any(|server| {
                    server.get("ok").and_then(serde_json::Value::as_bool) == Some(false)
                })
            });
    print_response_data(&request, response.data, mode, color);
    if failed {
        anyhow::bail!("One or more configured servers failed to start");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_transport_normalization_preserves_values_and_errors() -> anyhow::Result<()> {
        assert_eq!(normalize_transport("HTTP")?, "http");
        assert_eq!(normalize_transport("sSe")?, "sse");
        assert_eq!(
            normalize_transport("StDiO")
                .err()
                .ok_or_else(|| anyhow::anyhow!("invalid transport accepted"))?
                .to_string(),
            "invalid --transport 'stdio': expected 'http' or 'sse'"
        );
        let definition = build_server_def(
            Some("https://example.test/mcp".to_owned()),
            Some("SSE".to_owned()),
            None,
            Vec::new(),
        )?;
        assert_eq!(definition.transport, "sse");
        assert_eq!(definition.url, "https://example.test/mcp");
        Ok(())
    }
}
