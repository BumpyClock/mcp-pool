use std::collections::VecDeque;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::control::{ControlRequest, ControlResponse};
use crate::server_config::{ConfiguredServer, ServerConfiguration};
use crate::tool_arguments::{AdHoc, value};

#[path = "daemon_launch.rs"]
pub(crate) mod launch;
pub(crate) use crate::diagnostics::logging;
#[path = "daemon_migration.rs"]
mod migration;
#[path = "daemon_options.rs"]
mod options;

pub(crate) async fn auth_with_timeout(
    configuration: &ServerConfiguration,
    arguments: Vec<String>,
    oauth_timeout: Option<u64>,
) -> Result<()> {
    let mut arguments = VecDeque::from(arguments);
    let mut name = None;
    let mut ephemeral = AdHoc::default();
    let mut reset = false;
    let mut json_output = false;
    let mut no_browser = std::env::var("MCPORTER_OAUTH_NO_BROWSER")
        .ok()
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
    while let Some(argument) = arguments.pop_front() {
        if ephemeral.consume(&argument, &mut arguments)? {
            continue;
        }
        match argument.as_str() {
            "--reset" => reset = true,
            "--json" => json_output = true,
            "--no-browser" => no_browser = true,
            "--browser" => {
                if value(&mut arguments, &argument)? != "none" {
                    bail!("--browser supports 'none'; omit it to open the default browser");
                }
                no_browser = true;
            }
            "--yes" => {}
            _ if argument.starts_with('-') => bail!("Unknown auth flag '{argument}'"),
            _ if name.is_none() => name = Some(argument),
            _ => bail!("Usage: auth SERVER [--reset] [--no-browser] [--json]"),
        }
    }
    let name = name.as_deref().unwrap_or("adhoc");
    let server = crate::mcp_cli::server(configuration, name, &ephemeral)?;
    retire_pool_views(&server).await?;
    let result = crate::oauth::authorize_with_timeout(
        &server,
        crate::oauth::AuthOptions {
            reset,
            no_browser,
            json: json_output,
        },
        oauth_timeout,
    )
    .await?;
    crate::mcp_cli::persist_ad_hoc(&server, &ephemeral).await?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!("Authentication complete for '{}'", server.name);
    }
    Ok(())
}

pub async fn vault(configuration: &ServerConfiguration, arguments: Vec<String>) -> Result<()> {
    let mut arguments = VecDeque::from(arguments);
    let command = value(&mut arguments, "vault subcommand")?;
    let name = value(&mut arguments, "server name")?;
    let server = configuration
        .servers
        .get(&name)
        .with_context(|| format!("Unknown MCP server '{name}'"))?;
    match command.as_str() {
        "set" => {
            let mut path = None;
            let mut stdin = false;
            while let Some(argument) = arguments.pop_front() {
                match argument.as_str() {
                    "--tokens-file" => path = Some(value(&mut arguments, &argument)?),
                    "--stdin" => stdin = true,
                    _ => bail!("Unknown vault set argument '{argument}'"),
                }
            }
            if stdin == path.is_some() {
                bail!("Use exactly one of --tokens-file PATH or --stdin");
            }
            let contents = if stdin {
                crate::mcp_cli::stdin().await?
            } else {
                let path = path.context("Missing token file")?;
                if std::fs::metadata(&path)?.len() > 16 * 1024 * 1024 {
                    bail!("Credential input exceeds 16 MiB");
                }
                tokio::fs::read_to_string(path).await?
            };
            let payload: Value = serde_json::from_str(&contents)
                .map_err(|_| anyhow::anyhow!("Credential payload must be valid JSON"))?;
            retire_pool_views(server).await?;
            let selected = server.clone();
            tokio::task::spawn_blocking(move || crate::oauth::vault_set(&selected, &payload))
                .await
                .context("Credential write task failed")??;
            println!("Saved OAuth credentials for '{name}'");
        }
        "clear" => {
            if !arguments.is_empty() {
                bail!("Usage: vault clear SERVER");
            }
            clear_credentials(server).await?;
            println!("Cleared OAuth vault entry for '{name}'");
        }
        _ => bail!("Usage: vault set|clear SERVER"),
    }
    Ok(())
}

pub(crate) async fn clear_credentials(server: &ConfiguredServer) -> Result<()> {
    retire_pool_views(server).await?;
    let selected = server.clone();
    tokio::task::spawn_blocking(move || crate::oauth::vault_clear(&selected))
        .await
        .context("Credential clear task failed")?
}

pub(crate) async fn retire_pool_views(server: &ConfiguredServer) -> Result<()> {
    let entry = crate::config::ConfigurationEntry {
        source: server.source.clone(),
        name: server.name.clone(),
    };
    let Some(response) = direct(&ControlRequest::Status { name: None }).await? else {
        return Ok(());
    };
    let status = checked(response)?;
    retire_matching(&status, &entry, |request| async move {
        crate::daemon_client::control_request(&request).await
    })
    .await
}

async fn retire_matching<F, Response>(
    status: &Value,
    entry: &crate::config::ConfigurationEntry,
    mut control: F,
) -> Result<()>
where
    F: FnMut(ControlRequest) -> Response,
    Response: std::future::Future<Output = Result<ControlResponse>>,
{
    let active = status
        .get("servers")
        .and_then(Value::as_array)
        .context("Daemon status omitted servers; pool mutation was not attempted")?;
    for server in active {
        let name = server
            .get("name")
            .and_then(Value::as_str)
            .context("Daemon status server omitted its name")?;
        let associated = match server.get("configuration_entry") {
            None | Some(Value::Null) => None,
            Some(value) => Some(
                serde_json::from_value::<crate::config::ConfigurationEntry>(value.clone())
                    .context("Daemon status has invalid configuration-entry ownership metadata")?,
            ),
        };
        if associated.as_ref() == Some(entry) {
            if server.get("owned").and_then(Value::as_bool) != Some(true) {
                bail!(
                    "Pool mutation blocked: '{name}' is not a verified owned generation. Stop it through its owning runtime first."
                );
            }
            let response = control(ControlRequest::Stop {
                name: name.to_owned(),
            })
            .await?;
            if !response.ok {
                bail!(
                    "Pool mutation blocked: {}",
                    response
                        .error
                        .unwrap_or_else(|| "pool retirement was not verified".to_owned())
                );
            }
        }
    }
    Ok(())
}

pub async fn daemon(arguments: Vec<String>) -> Result<()> {
    let mut arguments = VecDeque::from(arguments);
    let command = arguments.pop_front().unwrap_or_else(|| "help".to_owned());
    if command == "help" {
        crate::mcp_cli::command_help("daemon");
        return Ok(());
    }
    if command == "migrate" {
        return migration::run(arguments).await;
    }
    let options = options::parse(&command, arguments)?;
    let json_output = options.json;
    let result = match command.as_str() {
        "status" => status().await?,
        "start" => {
            if let Some(response) = direct(&ControlRequest::Status { name: None }).await? {
                running(checked(response)?)
            } else {
                launch::start(&options).await?;
                status().await?
            }
        }
        "stop" => {
            stop().await?;
            json!({"running":false})
        }
        "restart" => {
            crate::diagnostics::log("daemon restart: retiring existing broker");
            stop().await?;
            crate::diagnostics::log("daemon restart: existing endpoint retired; starting broker");
            launch::start(&options).await?;
            crate::diagnostics::log("daemon restart: replacement ready");
            status().await?
        }
        _ => bail!("Unknown daemon subcommand '{command}'. Use daemon help"),
    };
    if json_output {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        let active = result
            .get("running")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        println!(
            "Daemon {} (mcp-pool broker)",
            if active { "running" } else { "stopped" }
        );
        if active && let Some(servers) = result.get("servers").and_then(Value::as_array) {
            for server in servers {
                println!(
                    "  {}: {}",
                    server
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("server"),
                    server
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                );
            }
        }
    }
    Ok(())
}

fn checked(response: ControlResponse) -> Result<Value> {
    if !response.ok {
        bail!(
            "{}",
            response
                .error
                .unwrap_or_else(|| "Daemon operation failed".to_owned())
        );
    }
    Ok(response.data.unwrap_or_else(|| json!({})))
}

fn running(mut data: Value) -> Value {
    if let Some(object) = data.as_object_mut() {
        object.insert("running".to_owned(), json!(true));
    } else {
        data = json!({"running":true,"data":data});
    }
    data
}

async fn status() -> Result<Value> {
    match direct(&ControlRequest::Status { name: None }).await? {
        Some(response) => Ok(running(checked(response)?)),
        None => Ok(json!({"running":false})),
    }
}

async fn stop() -> Result<()> {
    crate::diagnostics::log("daemon stop: requesting verified shutdown");
    if let Some(response) = direct(&ControlRequest::Shutdown).await? {
        checked(response)?;
    }
    crate::diagnostics::log("daemon stop: shutdown acknowledged; verifying endpoint disappearance");
    let expires = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        match crate::transport::connect(&crate::config::control_socket_path()).await {
            Ok(stream) => drop(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(());
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || (cfg!(windows) && error.raw_os_error() == Some(231)) => {}
            Err(error) => return Err(error).context("Verifying daemon endpoint retirement"),
        }
        if tokio::time::Instant::now() >= expires {
            bail!("Daemon retirement not verified within 15 seconds; replacement was not started");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn direct(request: &ControlRequest) -> Result<Option<ControlResponse>> {
    let path = crate::config::control_socket_path();
    tokio::time::timeout(Duration::from_secs(15), async {
        let stream = loop {
            match crate::transport::connect(&path).await {
                Ok(stream) => break stream,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    return Ok(None);
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || (cfg!(windows) && error.raw_os_error() == Some(231)) =>
                {
                    tokio::time::sleep(Duration::from_millis(10)).await
                }
                Err(error) => return Err(error).context("Connecting to existing daemon"),
            }
        };
        let mut stream = BufReader::new(stream);
        let mut serialized = serde_json::to_vec(request)?;
        serialized.push(b'\n');
        stream.get_mut().write_all(&serialized).await?;
        stream.get_mut().flush().await?;
        let mut line = String::new();
        if stream.read_line(&mut line).await? == 0 {
            bail!("Daemon closed without confirming the operation");
        }
        Ok(Some(
            serde_json::from_str(&line).context("Invalid daemon control response")?,
        ))
    })
    .await
    .context("Daemon control deadline exceeded")?
}

#[cfg(test)]
#[path = "credential_dispatch_tests.rs"]
mod tests;
