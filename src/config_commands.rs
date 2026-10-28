use std::collections::VecDeque;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::tool_arguments::value;

#[path = "config_writer.rs"]
pub(crate) mod write;
use write::write_checked_mutation;
#[path = "config_commands_copy.rs"]
mod copy;
#[path = "config_commands_input.rs"]
mod input;
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
        crate::mcp_cli::command_help("config");
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

#[derive(Clone, Copy)]
enum AddPolicy {
    Regular,
    Copy { replace: bool },
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
    let copying = matches!(policy, AddPolicy::Copy { .. });
    let replace = matches!(policy, AddPolicy::Copy { replace: true });
    let destination = crate::config_paths::destination(destination, root)?;
    if copying {
        crate::server_config::validate_document(&json!({"mcpServers":{&name:&entry}}))?;
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
    if dry_run {
        let mut document = write::read_document(&destination)?;
        insert_entry(&mut document, &name, &entry, true, replace, &destination)?;
        crate::server_config::validate_document(&document)?;
        println!("Would add '{name}' to {}", destination.display());
        return Ok(());
    }
    let source = destination.clone();
    let prior_name = name.clone();
    let changed = entry.clone();
    write_checked_mutation(
        &destination,
        |document| insert_entry(document, &name, &entry, copying, replace, &destination),
        move |document| async move {
            if !copying
                || document
                    .get("mcpServers")
                    .and_then(|servers| servers.get(&prior_name))
                    .is_some_and(|previous| previous != &changed)
            {
                retire_previous(&source, &document, &prior_name).await?;
            }
            Ok(())
        },
    )
    .await?;
    println!("Added '{name}' to {}", destination.display());
    Ok(())
}

fn insert_entry(
    document: &mut Value,
    name: &str,
    entry: &Value,
    copying: bool,
    replace: bool,
    destination: &std::path::Path,
) -> Result<()> {
    let servers = document
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .context("mcpServers must be an object")?;
    if copying && servers.get(name).is_some_and(|previous| previous != entry) && !replace {
        bail!(
            "Server '{name}' already exists in {}; use --replace to overwrite it. No changes made.",
            destination.display()
        );
    }
    servers.insert(name.to_owned(), entry.clone());
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
