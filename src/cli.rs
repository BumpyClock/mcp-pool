use clap::{Parser, Subcommand};

use crate::diagnostics;

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

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Run the pool daemon in the foreground (auto-launched by other commands if absent).
    Serve,

    /// Start a pooled MCP server by name (reads config, drives the daemon).
    Start { name: String },

    /// Stop a pooled MCP server.
    Stop { name: String },

    /// Restart a pooled MCP server.
    Restart { name: String },

    /// Show pool status (all, or one by name).
    Status { name: Option<String> },

    /// List configured servers.
    List,

    /// Add a server to config. Stdio: `add NAME -- COMMAND [ARGS...]`.
    /// Remote: `add NAME --url URL [--transport http|sse]`.
    Add {
        name: String,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        transport: Option<String>,
        #[arg(long)]
        command: Option<String>,
        /// Trailing command + args (after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        trailing: Vec<String>,
    },

    /// Remove a server from config.
    Remove { name: String },

    /// Bridge an agent's stdio to a pool socket (put this in the agent's MCP config).
    Proxy { name: String },
}

pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if cli.debug {
        diagnostics::set_enabled(true);
    }
    diagnostics::init_from_env();

    match cli.cmd {
        Cmd::Serve => crate::daemon::serve().await,
        Cmd::Proxy { name } => crate::proxy::run(&name).await,
        Cmd::Add {
            name,
            url,
            transport,
            command,
            trailing,
        } => add_server(name, url, transport, command, trailing),
        Cmd::Remove { name } => remove_server(name),
        Cmd::List => list_servers(),
        Cmd::Start { name } => control_round_trip(crate::control::ControlRequest::Start { name }).await,
        Cmd::Stop { name } => control_round_trip(crate::control::ControlRequest::Stop { name }).await,
        Cmd::Restart { name } => control_round_trip(crate::control::ControlRequest::Restart { name }).await,
        Cmd::Status { name } => control_round_trip(crate::control::ControlRequest::Status { name }).await,
    }
}

fn add_server(
    _name: String,
    _url: Option<String>,
    _transport: Option<String>,
    _command: Option<String>,
    _trailing: Vec<String>,
) -> anyhow::Result<()> {
    todo!("build ServerDef, PoolConfig::load -> upsert -> save, print confirmation")
}

fn remove_server(_name: String) -> anyhow::Result<()> {
    todo!("PoolConfig::load -> remove -> save")
}

fn list_servers() -> anyhow::Result<()> {
    todo!("PoolConfig::load -> print server table")
}

/// Send a control request to the daemon, auto-launching it if the control socket
/// is not reachable.
async fn control_round_trip(_request: crate::control::ControlRequest) -> anyhow::Result<()> {
    todo!("ensure_daemon (connect or spawn `serve` detached, retry), send request line, read+print ControlResponse")
}
