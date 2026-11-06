use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::tool_arguments::value;

#[path = "config_writer.rs"]
pub(crate) mod write;
use write::{WritePolicy, write_checked_mutation, write_planned_mutation};
#[path = "config_commands_copy.rs"]
mod copy;
#[path = "config_doctor.rs"]
mod doctor;
#[path = "config_commands_input.rs"]
mod input;
#[path = "config_commands_plan.rs"]
mod mutation;
use mutation::AddPolicy;
pub(crate) use mutation::EntryMutation;
#[path = "config_output.rs"]
mod output;
pub(crate) use output::serialize;
use output::summary;

pub(crate) async fn run_with_timeout(
    path: Option<PathBuf>,
    root: Option<&std::path::Path>,
    arguments: Vec<String>,
    oauth_timeout: Option<u64>,
) -> Result<()> {
    let mut arguments = VecDeque::from(arguments);
    let command = arguments.pop_front().unwrap_or_else(|| "help".to_owned());
    if command == "help" {
        crate::mcp_cli::command_help("config", &arguments.into_iter().collect::<Vec<_>>());
        return Ok(());
    }
    if command == "import" {
        return crate::config_import::run(path, root, arguments.into()).await;
    }
    if command == "add" {
        return add(input::parse_add(path, root, arguments)?, root).await;
    }
    if command == "remove" {
        let name = value(&mut arguments, "server name")?;
        if !arguments.is_empty() {
            bail!("Usage: config remove NAME");
        }
        return remove(path, root, &name).await;
    }
    let configuration = crate::server_config::load_at(path, root)?;
    for warning in &configuration.warnings {
        crate::mcp_cli::context::warning(warning);
    }
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
                bail!("Imported definitions are local copies; use --source local");
            }
            let servers: Vec<_> = configuration
                .servers
                .values()
                .filter(|server| {
                    positional.first().is_none_or(|filter| {
                        filter == "source:local" || server.name.contains(filter)
                    })
                })
                .collect();
            if json_output {
                let entries: Vec<_> = servers.into_iter().map(serialize).collect();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({"servers":entries}))?
                );
            } else {
                if servers.is_empty() {
                    println!("No local servers match the provided filters.");
                }
                for server in servers {
                    summary(server);
                }
                println!(
                    "Config: {}",
                    output::escape_text(&configuration.source.to_string_lossy())
                );
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
            if json_output {
                println!("{}", serde_json::to_string_pretty(&serialize(server))?);
            } else {
                summary(server);
            }
        }
        "doctor" => {
            if !positional.is_empty() {
                bail!("Usage: config doctor [--json]");
            }
            let credentials = doctor::inspect(&configuration).await?;
            if json_output {
                let servers: Vec<_> = credentials
                    .iter()
                    .map(|(server, status)| json!({"name":server.name,"credentials":status}))
                    .collect();
                let report = json!({"config":configuration.source,"valid":true,"serverCount":configuration.servers.len(),
                    "warnings":configuration.warnings,"servers":servers});
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!(
                    "Configuration valid: {} ({} servers)",
                    output::escape_text(&configuration.source.to_string_lossy()),
                    configuration.servers.len()
                );
                for (server, status) in &credentials {
                    println!(
                        "{}",
                        output::credential_summary(
                            &server.name,
                            server.oauth_settings().auth == Some("oauth"),
                            status
                        )
                    );
                }
            }
        }
        _ => bail!("Unknown config subcommand '{command}'. Use config help"),
    }
    Ok(())
}

pub(crate) async fn remove(
    path: Option<PathBuf>,
    root: Option<&std::path::Path>,
    name: &str,
) -> Result<()> {
    let path = crate::config_paths::destination(path, root)?;
    let source = path.clone();
    let prior_name = name.to_owned();
    write_checked_mutation(
        &path,
        WritePolicy::Replace,
        |document| remove_entry(document, name),
        move |document| async move { retire_previous(&source, &document, &prior_name).await },
    )
    .await?;
    println!("Removed '{name}' from {}", path.display());
    Ok(())
}

pub(crate) struct AddOperation {
    name: String,
    entry: Value,
    destination: Option<PathBuf>,
    dry_run: bool,
    policy: AddPolicy,
}

impl AddOperation {
    pub(crate) fn native(
        name: String,
        entry: Value,
        destination: Option<PathBuf>,
        dry_run: bool,
    ) -> Result<Self> {
        if name.is_empty() {
            bail!("Server name cannot be empty");
        }
        Ok(Self {
            name,
            entry,
            destination,
            dry_run,
            policy: AddPolicy::Regular,
        })
    }
}

pub(crate) async fn add(operation: AddOperation, root: Option<&std::path::Path>) -> Result<()> {
    let AddOperation {
        name,
        entry,
        destination,
        dry_run,
        policy,
    } = operation;
    let destination = crate::config_paths::destination(destination, root)?;
    if let AddPolicy::Copy { .. } = policy {
        crate::server_config::validate_new_entries(&json!({"mcpServers":{&name:&entry}}))?;
    } else {
        let validation = crate::server_config::parse_document(
            &destination,
            &json!({
                "mcpServers":{&name:&entry},"imports":[]
            }),
        )?;
        if dry_run {
            let server = validation
                .servers
                .get(&name)
                .context("Missing validated definition")?;
            println!("{}", serde_json::to_string_pretty(&serialize(server))?);
            return Ok(());
        }
    }
    let mutation = EntryMutation::new(serde_json::Map::from_iter([(name.clone(), entry)]), policy);
    if dry_run {
        let mut document = write::read_document(&destination)?;
        mutation.apply(&mut document, &destination)?;
        crate::server_config::validate_document(&document)?;
        println!("Would add '{name}' to {}", destination.display());
        return Ok(());
    }
    let source = destination.clone();
    write_planned_mutation(
        &destination,
        mutation.write_policy(),
        |document| mutation.apply(document, &destination),
        move |_, retirements| async move { retirements.retire(&source).await },
    )
    .await?;
    println!("Added '{name}' to {}", destination.display());
    Ok(())
}

pub(crate) async fn retire_previous(
    source: &std::path::Path,
    document: &Value,
    name: &str,
) -> Result<()> {
    if document
        .get("mcpServers")
        .and_then(|entries| entries.get(name))
        .is_some()
    {
        crate::daemon_commands::retire_configuration(source, name).await?;
    }
    Ok(())
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
}
