use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::tool_arguments::value;

pub(crate) async fn run(
    path: Option<PathBuf>,
    root: Option<&Path>,
    arguments: Vec<String>,
) -> Result<()> {
    let mut arguments = VecDeque::from(arguments);
    let source = value(
        &mut arguments,
        "source (claude, claude-desktop, codex, mcporter, cursor, windsurf, opencode, vscode, project, or mcp-pool)",
    )?;
    let mut from = None;
    let mut destination = path;
    let mut dry_run = false;
    let mut replace = false;
    let mut json_output = false;
    let mut include_auth = false;
    let mut names = Vec::new();
    while let Some(argument) = arguments.pop_front() {
        match argument.as_str() {
            "--from" => {
                from = Some(crate::config_paths::absolute(Path::new(&value(
                    &mut arguments,
                    &argument,
                )?))?)
            }
            "--scope" => {
                destination = Some(match value(&mut arguments, &argument)?.as_str() {
                    "home" | "user" => crate::config_paths::home_config()?,
                    "project" => crate::config_paths::project_config(root)?,
                    _ => bail!("--scope must be home or project"),
                });
            }
            "--dry-run" => dry_run = true,
            "--replace" => replace = true,
            "--json" => json_output = true,
            "--include-auth" => include_auth = true,
            _ if argument.starts_with('-') => bail!("Unknown import flag '{argument}'"),
            _ => names.push(argument),
        }
    }
    if include_auth && source != "mcporter" {
        bail!("--include-auth is supported only for mcporter imports");
    }
    let project = crate::config_paths::project_directory(root)?;
    let home = dirs::home_dir().context("Home directory unavailable")?;
    let mut batch =
        crate::config_import_sources::collect(&source, from.as_deref(), &project, &home, &names)?;
    for name in &names {
        if !batch.servers.contains_key(name) {
            bail!("Import source has no server named '{name}'");
        }
    }
    for warning in &batch.warnings {
        crate::mcp_cli::context::warning(warning);
    }
    // Imports are copies. External files are never part of runtime configuration.
    let destination = crate::config_paths::absolute(&match destination {
        Some(path) => path,
        None => crate::config_paths::explicit(None)?.unwrap_or(crate::config_paths::home_config()?),
    })?;
    let authentication = if include_auth {
        crate::config_import_auth::Plan::prepare(&mut batch, &destination, &home, replace)?
    } else {
        crate::config_import_auth::Plan::default()
    };
    let entries: serde_json::Map<String, Value> = batch
        .servers
        .iter()
        .map(|(name, server)| (name.clone(), server.entry.clone()))
        .collect();
    crate::server_config::validate_document(&json!({"mcpServers":entries}))?;
    let report = json!({
        "destination":destination,"dryRun":dry_run,
        "servers":batch.servers.iter().map(|(name, server)| {
            match authentication.statuses.get(name) {
                Some(status) => json!({"name":name,"source":server.source,"auth":status}),
                None => json!({"name":name,"source":server.source}),
            }
        }).collect::<Vec<_>>()
    });
    let mutate = |document: &mut Value| -> Result<()> {
        let servers = document
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .context("Destination needs an mcpServers object")?;
        for (name, entry) in &entries {
            if let Some(existing) = servers.get(name)
                && existing != entry
                && !replace
            {
                bail!(
                    "Server '{name}' already exists in {}; use --replace to overwrite it. No changes made.",
                    destination.display()
                );
            }
        }
        servers.extend(entries.clone());
        Ok(())
    };
    if dry_run {
        let mut document = crate::config_commands::write::read_document(&destination)?;
        mutate(&mut document)?;
        crate::server_config::validate_document(&document)?;
    } else {
        authentication.ensure_writable()?;
        let planned_authentication = &authentication;
        let previous_source = destination.clone();
        let changed = entries.clone();
        crate::config_commands::write::write_checked_mutation(
            &destination,
            mutate,
            move |document| async move {
                for (name, entry) in &changed {
                    if document
                        .get("mcpServers")
                        .and_then(|servers| servers.get(name))
                        .is_some_and(|old| old != entry)
                    {
                        crate::config_commands::retire_previous(&previous_source, &document, name)
                            .await?;
                    }
                }
                planned_authentication.retire().await
            },
        )
        .await?;
    }
    if include_auth {
        crate::mcp_cli::context::warning(
            "Refresh tokens may rotate. Stop using imported servers through mcporter after cutover; concurrent use can invalidate credentials.",
        );
    }
    if !dry_run {
        authentication.commit()?;
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "{} {} server(s) {} {}",
            if dry_run { "Would import" } else { "Imported" },
            entries.len(),
            if dry_run { "into" } else { "to" },
            destination.display()
        );
        for name in entries.keys() {
            if let Some(status) = authentication.statuses.get(name) {
                println!("  {name} (auth: {status})");
            } else {
                println!("  {name}");
            }
        }
    }
    Ok(())
}
