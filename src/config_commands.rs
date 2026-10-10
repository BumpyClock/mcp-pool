use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::tool_arguments::value;

#[path = "config_writer.rs"]
pub(crate) mod write;
use write::write_checked_mutation;
#[cfg(test)]
use write::write_mutation;
#[path = "config_output.rs"]
mod output;
pub(crate) use output::serialize;
use output::summary;

pub(crate) async fn run_with_timeout(
    path: Option<PathBuf>,
    arguments: Vec<String>,
    oauth_timeout: Option<u64>,
) -> Result<()> {
    let mut arguments = VecDeque::from(arguments);
    let command = arguments.pop_front().unwrap_or_else(|| "help".to_owned());
    if command == "help" {
        crate::mcp_cli::command_help("config");
        return Ok(());
    }
    if command == "import" || command == "migrate" {
        bail!(
            "Config import/migration is explicitly deferred. Read an existing mcporter.json directly with --config PATH or MCPORTER_CONFIG; use config add for explicit local changes."
        );
    }
    if command == "add" {
        return add(path, arguments).await;
    }
    if command == "remove" {
        let name = value(&mut arguments, "server name")?;
        if !arguments.is_empty() {
            bail!("Usage: config remove NAME");
        }
        let path = selected_path(path)?;
        let source = path.clone();
        let prior_name = name.clone();
        write_checked_mutation(
            &path,
            |document| remove_entry(document, &name),
            move |document| async move { retire_previous(&source, &document, &prior_name).await },
        )
        .await?;
        println!("Removed '{name}' from {}", path.display());
        return Ok(());
    }
    let configuration = crate::mcp_cli::load(path)?;
    if command == "login" {
        return crate::daemon_commands::auth_with_timeout(
            &configuration,
            arguments.into(),
            oauth_timeout,
        )
        .await;
    }
    if command == "logout" {
        let name = value(&mut arguments, "server name")?;
        if !arguments.is_empty() {
            bail!("Usage: config logout NAME");
        }
        let selected = configuration
            .servers
            .get(&name)
            .with_context(|| format!("Unknown MCP server '{name}'"))?;
        crate::daemon_commands::clear_credentials(selected).await?;
        println!("Cleared OAuth credentials for '{name}'");
        return Ok(());
    }
    let mut json_output = false;
    let mut source = None;
    let mut positional = Vec::new();
    while let Some(argument) = arguments.pop_front() {
        match argument.as_str() {
            "--json" => json_output = true,
            "--source" => source = Some(value(&mut arguments, &argument)?),
            _ if argument.starts_with('-') => bail!("Unknown config flag '{argument}'"),
            _ => positional.push(argument),
        }
    }
    if source
        .as_deref()
        .is_some_and(|source| !matches!(source, "local" | "import"))
    {
        bail!("--source must be local or import");
    }
    match command.as_str() {
        "list" => {
            if positional.len() > 1 {
                bail!("Usage: config list [FILTER] [--json] [--source local|import]");
            }
            if source.as_deref() == Some("import") {
                bail!(
                    "Editor config imports are deferred; use --config PATH to inspect local definitions directly"
                );
            }
            let entries: Vec<Value> = configuration
                .servers
                .values()
                .filter(|server| {
                    positional.first().is_none_or(|filter| {
                        filter == "source:local" || server.name.contains(filter)
                    })
                })
                .map(serialize)
                .collect();
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"servers":entries}))?
                );
            } else {
                if entries.is_empty() {
                    println!("No local servers match the provided filters.");
                }
                for entry in &entries {
                    summary(entry);
                }
                println!("Config: {}", configuration.source.display());
            }
        }
        "get" => {
            if positional.len() != 1 {
                bail!("Usage: config get NAME [--json]");
            }
            let name = positional.first().context("Missing server")?;
            let server = configuration
                .servers
                .get(name)
                .with_context(|| format!("Unknown MCP server '{name}'"))?;
            let entry = serialize(server);
            if json_output {
                println!("{}", serde_json::to_string_pretty(&entry)?);
            } else {
                summary(&entry);
            }
        }
        "doctor" => {
            if !positional.is_empty() {
                bail!("Usage: config doctor [--json]");
            }
            let mut credentials = Vec::new();
            for server in configuration.servers.values() {
                let selected = server.clone();
                let status =
                    tokio::task::spawn_blocking(move || crate::oauth::credential_status(&selected))
                        .await
                        .context("Credential inspection task failed")??;
                credentials.push(json!({"name":server.name,"credentials":status}));
            }
            let report = json!({"config":configuration.source,"valid":true,"serverCount":configuration.servers.len(),
                "warnings":configuration.warnings,"servers":credentials});
            if json_output {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Configuration valid: {} ({} servers)",
                    configuration.source.display(),
                    configuration.servers.len()
                );
                for entry in credentials {
                    println!("{}", serde_json::to_string(&entry)?);
                }
            }
        }
        _ => bail!("Unknown config subcommand '{command}'. Use config help"),
    }
    Ok(())
}

async fn add(path: Option<PathBuf>, mut arguments: VecDeque<String>) -> Result<()> {
    let mut name = None;
    let mut target = None;
    let mut entry = json!({});
    let mut command = None;
    let mut url = None;
    let mut transport = None;
    let mut passed = Vec::new();
    let mut environment = serde_json::Map::new();
    let mut headers = serde_json::Map::new();
    let mut destination = path;
    let mut dry_run = false;
    while let Some(argument) = arguments.pop_front() {
        match argument.as_str() {
            "--" => {
                passed.extend(arguments);
                break;
            }
            "--command" | "--stdio" => command = Some(value(&mut arguments, &argument)?),
            "--url" => url = Some(value(&mut arguments, &argument)?),
            "--transport" => transport = Some(value(&mut arguments, &argument)?),
            "--arg" => passed.push(value(&mut arguments, &argument)?),
            "--persist" => destination = Some(PathBuf::from(value(&mut arguments, &argument)?)),
            "--scope" => {
                let scope = value(&mut arguments, &argument)?;
                destination = Some(match scope.as_str() {
                    "home" => default_path()?,
                    "project" => std::env::current_dir()?
                        .join("config")
                        .join("mcporter.json"),
                    _ => bail!("--scope must be home or project"),
                });
            }
            "--dry-run" => dry_run = true,
            "--env" | "--header" => {
                let content = value(&mut arguments, &argument)?;
                let (key, content) = content
                    .split_once('=')
                    .or_else(|| content.split_once(':'))
                    .context("--env/--header requires KEY=value")?;
                if key.trim().is_empty() {
                    bail!("Empty environment/header name");
                }
                let values = if argument == "--env" {
                    &mut environment
                } else {
                    &mut headers
                };
                values.insert(key.trim().to_owned(), json!(content.trim()));
            }
            "--description"
            | "--auth"
            | "--token-cache-dir"
            | "--client-name"
            | "--oauth-client-id"
            | "--oauth-client-secret-env"
            | "--oauth-token-endpoint-auth-method"
            | "--oauth-redirect-url"
            | "--oauth-requested-scope" => {
                let key = match argument.as_str() {
                    "--description" => "description",
                    "--auth" => "auth",
                    "--token-cache-dir" => "tokenCacheDir",
                    "--client-name" => "clientName",
                    "--oauth-client-id" => "oauthClientId",
                    "--oauth-client-secret-env" => "oauthClientSecretEnv",
                    "--oauth-token-endpoint-auth-method" => "oauthTokenEndpointAuthMethod",
                    "--oauth-redirect-url" => "oauthRedirectUrl",
                    _ => "oauthRequestedScope",
                };
                if let Some(object) = entry.as_object_mut() {
                    object.insert(key.to_owned(), json!(value(&mut arguments, &argument)?));
                }
            }
            "--copy-from" => bail!(
                "Imported definitions are deferred; use --config PATH and config add explicitly"
            ),
            _ if argument.starts_with('-') => bail!("Unknown config add flag '{argument}'"),
            _ if name.is_none() => name = Some(argument),
            _ if target.is_none() => target = Some(argument),
            _ => bail!(
                "Unexpected config add argument; use --arg VALUE or -- to pass stdio arguments"
            ),
        }
    }
    let name = name.context("Usage: config add NAME [URL] [--command COMMAND]")?;
    if name.is_empty() {
        bail!("Server name cannot be empty");
    }
    if let Some(target) = target {
        if command.is_some() || url.is_some() {
            bail!("Specify either a positional target or --command/--url");
        }
        if target.starts_with("http://") || target.starts_with("https://") {
            url = Some(target);
        } else {
            command = Some(target);
        }
    }
    if command.is_some() == url.is_some() {
        bail!("Specify exactly one HTTP URL or stdio command");
    }
    let kind = transport
        .as_deref()
        .unwrap_or(if url.is_some() { "http" } else { "stdio" });
    if !matches!(kind, "http" | "sse" | "stdio") || (kind == "stdio") != command.is_some() {
        bail!("--transport must agree with URL/command");
    }
    if let Some(object) = entry.as_object_mut() {
        object.insert("transport".to_owned(), json!(kind));
        if let Some(url) = url {
            object.insert("baseUrl".to_owned(), json!(url));
        }
        if let Some(command) = command {
            object.insert("command".to_owned(), json!(command));
            object.insert("args".to_owned(), json!(passed));
        }
        if !environment.is_empty() {
            object.insert("env".to_owned(), Value::Object(environment));
        }
        if !headers.is_empty() {
            object.insert("headers".to_owned(), Value::Object(headers));
        }
    }
    let destination = selected_path(destination)?;
    let validation = crate::server_config::parse_config(
        &destination,
        &serde_json::to_string(&json!({
            "mcpServers":{&name:&entry},"imports":[]
        }))?,
    )?;
    if dry_run {
        let server = validation
            .servers
            .get(&name)
            .context("Missing validated definition")?;
        println!("{}", serde_json::to_string_pretty(&serialize(server))?);
        return Ok(());
    }
    let source = destination.clone();
    let prior_name = name.clone();
    write_checked_mutation(
        &destination,
        |document| {
            let servers = document
                .get_mut("mcpServers")
                .and_then(Value::as_object_mut)
                .context("mcpServers must be an object")?;
            servers.insert(name.clone(), entry.clone());
            Ok(())
        },
        move |document| async move { retire_previous(&source, &document, &prior_name).await },
    )
    .await?;
    println!("Added '{name}' to {}", destination.display());
    Ok(())
}

fn default_path() -> Result<PathBuf> {
    Ok(dirs::home_dir()
        .context("Home directory unavailable")?
        .join(".mcporter")
        .join("mcporter.json"))
}

pub(crate) async fn retire_previous(
    source: &std::path::Path,
    document: &Value,
    name: &str,
) -> Result<()> {
    let previous = crate::server_config::parse_config(source, &serde_json::to_string(document)?)?;
    if let Some(server) = previous.servers.get(name) {
        crate::daemon_commands::retire_pool_views(server).await?;
    }
    Ok(())
}

fn selected_path(path: Option<PathBuf>) -> Result<PathBuf> {
    let selected = match path {
        Some(path) => path,
        None => match std::env::var_os("MCPORTER_CONFIG") {
            Some(path) => PathBuf::from(path),
            None => default_path()?,
        },
    };
    let text = selected.to_string_lossy();
    let expanded =
        if let Some(relative) = text.strip_prefix("~/").or_else(|| text.strip_prefix("~\\")) {
            dirs::home_dir()
                .context("Home directory unavailable")?
                .join(relative)
        } else {
            selected
        };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        Ok(std::env::current_dir()?.join(expanded))
    }
}

fn remove_entry(document: &mut Value, name: &str) -> Result<()> {
    let servers = document
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .context("mcpServers must be an object")?;
    if servers.remove(name).is_none() {
        bail!("No local entry named '{name}'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removal_preserves_other_servers_and_unrelated_config_fields() -> Result<()> {
        let mut document = json!({"mcpServers":{"one":{"command":"echo"},"two":{"command":"echo"}},"imports":["cursor"],"custom":{"enabled":true}});
        remove_entry(&mut document, "one")?;
        assert_eq!(
            document,
            json!({"mcpServers":{"two":{"command":"echo"}},"imports":["cursor"],"custom":{"enabled":true}})
        );
        assert!(remove_entry(&mut document, "absent").is_err());
        Ok(())
    }

    #[tokio::test]
    async fn atomic_mutation_is_isolated_and_preserves_keys() -> Result<()> {
        let identity = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let directory = std::env::current_dir()?
            .join("target")
            .join(format!("configuration-writer-{identity}"));
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("config.json");
        let result = async {
            std::fs::write(
                &path,
                "{\"mcpServers\":{\"one\":{\"command\":\"echo\"}},\"imports\":[],\"custom\":4}",
            )?;
            write_mutation(&path, |document| remove_entry(document, "one")).await?;
            let document: Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
            assert_eq!(document.get("custom"), Some(&json!(4)));
            assert_eq!(document.get("mcpServers"), Some(&json!({})));
            Ok::<(), anyhow::Error>(())
        }
        .await;
        std::fs::remove_dir_all(&directory)?;
        result
    }
}
