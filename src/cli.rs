use std::io::{self, IsTerminal, Write};

use clap::{Parser, Subcommand};

use crate::config::{self, PoolConfig, ServerDef};
use crate::control::ControlRequest;
use crate::daemon_client::control_request;
use crate::diagnostics;

#[path = "cli_output.rs"]
mod output;
use output::print_response_data;

#[derive(Parser)]
#[command(
    name = "mcp-pool",
    version,
    about = "Pool MCP servers — one upstream, many clients"
)]
pub struct Cli {
    /// Enable diagnostic logging (also via MCP_POOL_DEBUG=1).
    #[arg(long, global = true)]
    pub debug: bool,

    /// Machine-readable JSON output (status / list).
    #[arg(long, global = true)]
    pub json: bool,

    /// Stable line-based output: name<TAB>status<TAB>transport<TAB>socket.
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
    /// Run the pool daemon in the foreground (auto-launched by other commands if absent).
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

    /// List configured servers (local config; no daemon required).
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
    let arguments: Vec<String> = std::env::args_os()
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| anyhow::anyhow!("command-line arguments must be valid Unicode"))
        })
        .collect::<anyhow::Result<_>>()?;
    let command_position = crate::mcp_cli::command_position(arguments.get(1..).unwrap_or_default())
        .map(|position| position + 1);
    let native_namespace = command_position
        .and_then(|position| arguments.get(position))
        .is_some_and(|argument| argument == "pool");
    if !native_namespace && crate::mcp_cli::handles(arguments.get(1..).unwrap_or_default()) {
        diagnostics::init_from_env();
        return crate::mcp_cli::run(arguments.into_iter().skip(1).collect()).await;
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
    let cli = Cli::parse_from(native_arguments);

    diagnostics::init_from_env();
    if cli.debug {
        diagnostics::set_enabled(true);
    }

    let mode = output_mode(&cli);
    let color = use_color(&cli);

    match cli.cmd {
        Cmd::Serve => crate::daemon::serve().await,
        Cmd::Proxy { name } => {
            let native_config = PoolConfig::load()?;
            let allow_reference = std::env::var_os("MCP_POOL_HOME").is_none()
                || std::env::var_os("MCPORTER_CONFIG").is_some();
            if !native_namespace && allow_reference && !native_config.server.contains_key(&name) {
                let configuration = crate::server_config::load(None)?;
                let server = configuration
                    .servers
                    .get(&name)
                    .ok_or_else(|| anyhow::anyhow!("unknown server: {name}"))?;
                let definition = crate::oauth::prepare(server, false).await?;
                let pool_name = crate::mcp_cli::pool_name(server, &definition)?;
                crate::proxy::run_resolved(&pool_name, &definition).await
            } else {
                crate::proxy::run(&name).await
            }
        }
        Cmd::Add {
            name,
            url,
            transport,
            command,
            dry_run,
            trailing,
        } => add_server(&name, url, transport, command, dry_run, trailing),
        Cmd::Remove { name, yes } => remove_server(&name, yes),
        Cmd::List => list_servers(mode),
        Cmd::Start { name } => {
            let request = match name {
                Some(name) => ControlRequest::Start { name },
                None => ControlRequest::StartAll,
            };
            control_round_trip(request, mode, color).await
        }
        Cmd::Stop { name } => control_round_trip(ControlRequest::Stop { name }, mode, color).await,
        Cmd::Restart { name } => {
            control_round_trip(ControlRequest::Restart { name }, mode, color).await
        }
        Cmd::Status { name } => {
            control_round_trip(ControlRequest::Status { name }, mode, color).await
        }
        Cmd::Shutdown => control_round_trip(ControlRequest::Shutdown, mode, color).await,
    }
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

fn build_server_def(
    url: Option<String>,
    transport: Option<String>,
    command: Option<String>,
    trailing: Vec<String>,
) -> anyhow::Result<ServerDef> {
    let (stdio_command, args) = match (command, trailing.split_first()) {
        (Some(command), None) => (command, Vec::new()),
        (Some(_), Some(_)) => {
            return Err(anyhow::anyhow!(
                "--command cannot be combined with trailing command args"
            ));
        }
        (None, None) => (String::new(), Vec::new()),
        (None, Some((first, rest))) => {
            if first.is_empty() {
                return Err(anyhow::anyhow!("stdio command must not be empty"));
            }
            (first.clone(), rest.to_vec())
        }
    };
    let has_stdio = !stdio_command.is_empty();

    match (url, has_stdio) {
        (Some(_), true) | (None, false) => Err(anyhow::anyhow!(
            "specify exactly one of: --url <URL>  OR  a stdio command (-- COMMAND...)"
        )),
        (Some(url), false) => {
            let transport = transport
                .map(|value| normalize_transport(&value))
                .transpose()?
                .unwrap_or_else(|| "http".to_string());
            Ok(ServerDef {
                url,
                transport,
                ..Default::default()
            })
        }
        (None, true) => Ok(ServerDef {
            command: stdio_command,
            args,
            ..Default::default()
        }),
    }
}

fn normalize_transport(value: &str) -> anyhow::Result<String> {
    let normalized = value.to_ascii_lowercase();
    match normalized.as_str() {
        "http" | "sse" => Ok(normalized),
        other => Err(anyhow::anyhow!(
            "invalid --transport '{other}': expected 'http' or 'sse'"
        )),
    }
}

fn add_server(
    name: &str,
    url: Option<String>,
    transport: Option<String>,
    command: Option<String>,
    dry_run: bool,
    trailing: Vec<String>,
) -> anyhow::Result<()> {
    let server_def = build_server_def(url, transport, command, trailing)?;
    let target_path = config::config_path()?;

    if dry_run {
        let mut snapshot = PoolConfig::load().unwrap_or_default();
        snapshot.upsert(name, server_def);
        let toml_text = toml::to_string_pretty(&snapshot)
            .map_err(|error| anyhow::anyhow!("serialize config: {error}"))?;
        println!("# target: {}", target_path.display());
        print!("{toml_text}");
        return Ok(());
    }

    let mut pool_config = PoolConfig::load()?;
    pool_config.upsert(name, server_def);
    pool_config.save()?;

    println!("added server '{name}' -> {}", target_path.display());
    Ok(())
}

fn remove_server(name: &str, yes: bool) -> anyhow::Result<()> {
    let mut pool_config = PoolConfig::load()?;
    if !pool_config.server.contains_key(name) {
        eprintln!("mcp-pool: '{name}' is not configured");
        std::process::exit(1);
    }

    if !yes && io::stdin().is_terminal() && !confirm(&format!("remove server '{name}'?")) {
        println!("aborted");
        return Ok(());
    }

    pool_config.remove(name);
    pool_config.save()?;

    println!("removed server '{name}'");
    Ok(())
}

fn confirm(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    if io::stdout().flush().is_err() {
        return false;
    }
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Plain output is `name<TAB>status<TAB>transport<TAB>socket`; local config emits `-` for status.
fn list_servers(mode: OutputMode) -> anyhow::Result<()> {
    let pool_config = PoolConfig::load()?;
    let entries: Vec<(String, String, String)> = pool_config
        .server
        .iter()
        .map(|(name, def)| {
            (
                name.clone(),
                def.transport_kind().to_string(),
                config::server_socket_path(name)
                    .to_string_lossy()
                    .to_string(),
            )
        })
        .collect();

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
    request: ControlRequest,
    mode: OutputMode,
    color: bool,
) -> anyhow::Result<()> {
    let response = control_request(&request).await?;

    if !response.ok {
        let message = response
            .error
            .unwrap_or_else(|| "unknown error".to_string());
        eprintln!("mcp-pool: {message}");
        std::process::exit(1);
    }

    print_response_data(&request, response.data, mode, color);
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
