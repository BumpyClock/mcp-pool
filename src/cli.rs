use std::io::{self, IsTerminal, Write};
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::{self, PoolConfig, ServerDef};
use crate::control::{ControlRequest, ControlResponse};
use crate::diagnostics;
use crate::transport;

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

    /// Start a pooled MCP server by name (reads config, drives the daemon).
    Start { name: String },

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
    let cli = Cli::parse();

    if cli.debug {
        diagnostics::set_enabled(true);
    }
    diagnostics::init_from_env();

    let mode = output_mode(&cli);
    let color = use_color(&cli);

    match cli.cmd {
        Cmd::Serve => crate::daemon::serve().await,
        Cmd::Proxy { name } => crate::proxy::run(&name).await,
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
            control_round_trip(ControlRequest::Start { name }, mode, color).await
        }
        Cmd::Stop { name } => {
            control_round_trip(ControlRequest::Stop { name }, mode, color).await
        }
        Cmd::Restart { name } => {
            control_round_trip(ControlRequest::Restart { name }, mode, color).await
        }
        Cmd::Status { name } => {
            control_round_trip(ControlRequest::Status { name }, mode, color).await
        }
        Cmd::Shutdown => {
            control_round_trip(ControlRequest::Shutdown, mode, color).await
        }
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
    // Resolve the stdio command. Explicit --command is exclusive with trailing args.
    let (stdio_command, args) = match (command, trailing.split_first()) {
        (Some(command), None) => (command, Vec::new()),
        (Some(_), Some(_)) => {
            return Err(anyhow::anyhow!("--command cannot be combined with trailing command args"));
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

    // Exactly one source allowed.
    match (url, has_stdio) {
        (Some(_), true) | (None, false) => Err(anyhow::anyhow!(
            "specify exactly one of: --url <URL>  OR  a stdio command (-- COMMAND...)"
        )),
        (Some(url), false) => {
            let transport = transport
                .map(|value| normalize_transport(&value))
                .transpose()?
                .unwrap_or_else(|| "http".to_string());
            Ok(ServerDef { url, transport, ..Default::default() })
        }
        (None, true) => Ok(ServerDef { command: stdio_command, args, ..Default::default() }),
    }
}

fn normalize_transport(value: &str) -> anyhow::Result<String> {
    match value.to_ascii_lowercase().as_str() {
        "http" | "sse" => Ok(value.to_ascii_lowercase()),
        other => Err(anyhow::anyhow!("invalid --transport '{other}': expected 'http' or 'sse'")),
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

    if !yes && io::stdin().is_terminal()
        && !confirm(&format!("remove server '{name}'?")) {
            println!("aborted");
            return Ok(());
        }

    let removed = pool_config.remove(name);
    pool_config.save()?;

    if removed {
        println!("removed server '{name}'");
    } else {
        eprintln!("mcp-pool: '{name}' is not configured");
        std::process::exit(1);
    }
    Ok(())
}

/// Read a y/N confirmation from stdin. Defaults to No on anything but explicit yes.
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

fn list_servers(mode: OutputMode) -> anyhow::Result<()> {
    let pool_config = PoolConfig::load()?;
    let entries: Vec<(String, String, String)> = pool_config
        .server
        .iter()
        .map(|(name, def)| {
            (
                name.clone(),
                def.transport_kind().to_string(),
                config::server_socket_path(name).to_string_lossy().to_string(),
            )
        })
        .collect();

    if entries.is_empty() {
        if mode == OutputMode::Json { println!("[]") } else { println!("no servers configured") }
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
        // Plain format per contract: name<TAB>status<TAB>transport<TAB>socket. Local
        // config has no runtime status, so emit "-" there.
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

/// Send a control request to the daemon, auto-launching it if the control socket
/// is not reachable. Prints the response per the active output mode.
async fn control_round_trip(
    request: ControlRequest,
    mode: OutputMode,
    color: bool,
) -> anyhow::Result<()> {
    let response = send_control(&request).await?;

    if !response.ok {
        let message = response.error.unwrap_or_else(|| "unknown error".to_string());
        eprintln!("mcp-pool: {message}");
        std::process::exit(1);
    }

    print_response_data(&request, response.data, mode, color);
    Ok(())
}

/// Ensure a named server's upstream is started, auto-launching the daemon if
/// needed. Idempotent (a no-op when already running) and writes nothing to
/// stdout, so the `proxy` bridge can call it without corrupting its byte stream.
pub(crate) async fn ensure_started(name: &str) -> anyhow::Result<()> {
    let response = send_control(&ControlRequest::Start {
        name: name.to_string(),
    })
    .await?;
    if response.ok {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            response.error.unwrap_or_else(|| "unknown error".to_string())
        ))
    }
}

/// Serialize a control request, send it (retrying once on an empty teardown
/// response), and parse the daemon's reply.
async fn send_control(request: &ControlRequest) -> anyhow::Result<ControlResponse> {
    let mut request_line = serde_json::to_string(request)?;
    request_line.push('\n');

    // The control socket can briefly hand a connection to a tearing-down daemon
    // (notably Windows named pipes right after shutdown/restart). Retry once
    // before treating an empty response as a hard failure.
    let response_line = match send_request(&request_line).await? {
        Some(line) => line,
        None => {
            diagnostics::log("control response empty; retrying once");
            send_request(&request_line)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!("daemon closed control socket without responding")
                })?
        }
    };

    serde_json::from_str(response_line.trim())
        .map_err(|error| anyhow::anyhow!("parse control response: {error}"))
}

/// Send one framed request line and read one response line. Returns `None` when
/// the daemon accepted the connection but closed it without responding — a
/// teardown race the caller retries once.
async fn send_request(request_line: &str) -> anyhow::Result<Option<String>> {
    let mut stream = ensure_daemon().await?;
    stream.write_all(request_line.as_bytes()).await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    let bytes = reader.read_line(&mut response_line).await?;
    Ok(if bytes == 0 { None } else { Some(response_line) })
}

fn print_response_data(
    request: &ControlRequest,
    data: Option<serde_json::Value>,
    mode: OutputMode,
    color: bool,
) {
    if mode == OutputMode::Json {
        let text = data
            .and_then(|value| serde_json::to_string_pretty(&value).ok())
            .unwrap_or_else(|| "{}".to_string());
        println!("{text}");
        return;
    }
    let (green, yellow, _red, reset) = colors(color);

    match request {
        ControlRequest::Start { name } => println!("{green}started{reset} {name}"),
        ControlRequest::Stop { name } => println!("{yellow}stopped{reset} {name}"),
        ControlRequest::Restart { name } => println!("{green}restarted{reset} {name}"),
        ControlRequest::Shutdown => println!("{yellow}daemon shutting down{reset}"),
        ControlRequest::Status { name } => print_status_table(data.as_ref(), name.as_deref(), color),
    }
}

fn colors(color: bool) -> (&'static str, &'static str, &'static str, &'static str) {
    if color {
        ("\x1b[32m", "\x1b[33m", "\x1b[31m", "\x1b[0m")
    } else {
        ("", "", "", "")
    }
}

fn str_field<'a>(value: &'a serde_json::Value, key: &str) -> &'a str {
    value.get(key).and_then(|v| v.as_str()).unwrap_or("?")
}

fn print_status_table(data: Option<&serde_json::Value>, filter: Option<&str>, color: bool) {
    let (green, yellow, red, reset) = colors(color);
    let servers = data
        .and_then(|value| value.get("servers"))
        .and_then(|value| value.as_array())
        .map(|arr| arr.as_slice())
        .unwrap_or(&[]);

    let matches = |server: &serde_json::Value| match filter {
        Some(target) => str_field(server, "name") == target,
        None => true,
    };
    let shown: Vec<&serde_json::Value> = servers.iter().filter(|server| matches(server)).collect();

    if shown.is_empty() {
        match filter {
            Some(name) => println!("server '{name}' is not running"),
            None => println!("no servers running"),
        }
        return;
    }

    println!("{:<20} {:<10} {:<10} {:<6} SOCKET", "NAME", "STATUS", "TRANSPORT", "CONNS");
    for server in shown {
        let status = str_field(server, "status");
        let (prefix, suffix) = match status {
            "running" => (green, reset),
            "starting" => (yellow, reset),
            "stopped" => (red, reset),
            _ => ("", ""),
        };
        let connections = server.get("connection_count").and_then(|v| v.as_u64()).unwrap_or(0);
        println!(
            "{:<20} {prefix}{:<10}{suffix} {:<10} {:<6} {}",
            str_field(server, "name"),
            status,
            str_field(server, "transport"),
            connections,
            str_field(server, "socket_path"),
        );
    }
}

/// Connect to the control socket. If unreachable, spawn the daemon detached and
/// retry for a short window before giving up.
async fn ensure_daemon() -> anyhow::Result<transport::LocalStream> {
    let socket_path = config::control_socket_path();
    match transport::connect(&socket_path).await {
        Ok(stream) => Ok(stream),
        Err(_) => {
            spawn_daemon_detached()?;
            retry_connect(&socket_path).await
        }
    }
}

async fn retry_connect(socket_path: &std::path::Path) -> anyhow::Result<transport::LocalStream> {
    let mut last_error: Option<std::io::Error> = None;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match transport::connect(socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    let error = last_error
        .unwrap_or_else(|| std::io::Error::other("control socket unreachable"));
    Err(anyhow::anyhow!(
        "could not reach daemon after launch ({}). Try `mcp-pool serve` manually.",
        error
    ))
}

// The daemon is spawned fire-and-forget as a detached process, not an
// async-managed child, so std::process::Command (not tokio's) is correct here.
#[allow(clippy::disallowed_methods)]
fn spawn_daemon_detached() -> anyhow::Result<()> {
    let executable = std::env::current_exe()?;
    let mut command = std::process::Command::new(&executable);
    command.arg("serve");
    configure_detached(&mut command);
    command.spawn()?;
    diagnostics::log(format!("spawned daemon: {} serve", executable.display()));
    Ok(())
}

#[cfg(windows)]
#[allow(clippy::disallowed_methods)]
fn configure_detached(command: &mut std::process::Command) {
    use std::os::windows::process::CommandExt;
    // CREATE_NO_WINDOW (0x08000000): the daemon runs headless on Windows.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
}

#[cfg(unix)]
#[allow(clippy::disallowed_methods)]
fn configure_detached(command: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // New process group so the daemon survives the CLI's controlling terminal.
    command.process_group(0);
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
}
