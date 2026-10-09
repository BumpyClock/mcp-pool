use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::server_config::ConfiguredServer;
use crate::tool_arguments::AdHoc;

pub(super) fn destination(path: &std::path::Path) -> Result<std::path::PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

pub(crate) async fn persist(server: &ConfiguredServer, options: &AdHoc) -> Result<()> {
    let Some(path) = &options.persist else {
        return Ok(());
    };
    if !options.present() {
        bail!("--persist requires an ad-hoc --stdio or HTTP target");
    }
    let path = destination(path)?;
    let name = server.name.clone();
    let entry = persisted_entry(server)?;
    let source = path.clone();
    let previous_name = name.clone();
    crate::config_commands::write::write_checked_mutation(
        &path,
        move |document| {
            let object = document
                .as_object_mut()
                .context("Persistence config must be an object")?;
            let servers = object
                .entry("mcpServers")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .context("mcpServers must be an object")?;
            servers.insert(name, entry);
            Ok(())
        },
        move |document| async move {
            crate::config_commands::retire_previous(&source, &document, &previous_name).await
        },
    )
    .await?;
    eprintln!("[mcp-pool] Saved ad-hoc server to {}", path.display());
    Ok(())
}

fn persisted_entry(server: &ConfiguredServer) -> Result<Value> {
    let mut entry = server.raw.clone();
    if let Some(object) = entry.as_object_mut()
        && !server.definition.is_remote()
    {
        let arguments = object
            .get("command")
            .and_then(Value::as_array)
            .map(|tokens| Value::Array(tokens.iter().skip(1).cloned().collect()))
            .or_else(|| object.get("args").cloned())
            .unwrap_or_else(|| serde_json::json!(server.definition.args));
        object.insert(
            "command".to_owned(),
            Value::String(server.definition.command.clone()),
        );
        object.insert("args".to_owned(), arguments);
        if let Some(cwd) = &server.definition.cwd {
            object.insert("cwd".to_owned(), serde_json::json!(cwd));
        }
    }
    Ok(entry)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn persistence_keeps_original_executable_cwd_and_final_entry_identity() -> Result<()> {
        let configuration = crate::server_config::parse_config(
            &std::path::PathBuf::from("synthetic.json"),
            r#"{"imports":[],"mcpServers":{}}"#,
        )?;
        let executable = std::path::PathBuf::from(".")
            .join("synthetic-tools")
            .join("fixture.exe");
        let selected = crate::mcp_cli::server(
            &configuration,
            "hint",
            &AdHoc {
                command: Some(format!("\"{}\" literal", executable.display())),
                name: Some("persisted".to_owned()),
                persist: Some(
                    std::path::PathBuf::from("target")
                        .join("different-source")
                        .join("config.json"),
                ),
                ..AdHoc::default()
            },
        )?;
        let persisted = persisted_entry(&selected)?;
        let reloaded = crate::server_config::parse_config(
            &selected.source,
            &serde_json::to_string(
                &json!({"imports":[],"mcpServers":{selected.name.clone():persisted}}),
            )?,
        )?;
        let reloaded = reloaded.servers.get("persisted").context("persisted")?;
        assert_eq!(selected.definition.command, reloaded.definition.command);
        assert!(std::path::Path::new(&reloaded.definition.command).is_absolute());
        assert_eq!(selected.definition.cwd, reloaded.definition.cwd);
        assert_eq!(selected.definition.cwd, Some(std::env::current_dir()?));
        assert_eq!(
            crate::mcp_cli::pool_name(&selected, &selected.definition)?,
            crate::mcp_cli::pool_name(reloaded, &reloaded.definition)?
        );
        Ok(())
    }

    #[test]
    fn persistence_does_not_capture_expanded_argument_secrets_or_ambient_environment() -> Result<()>
    {
        let selected = ConfiguredServer {
            name: "fixture".to_owned(),
            source: "synthetic.json".into(),
            raw: json!({"command":["fixture","${CALLER_BINDING}"],"env":{"DECLARED":"explicit"}}),
            definition: crate::config::ServerDef {
                command: "fixture".to_owned(),
                args: vec!["resolved-private".to_owned()],
                env: std::collections::BTreeMap::from([(
                    "UNDECLARED".to_owned(),
                    "ambient-private".to_owned(),
                )]),
                ..crate::config::ServerDef::default()
            },
        };
        let persisted = persisted_entry(&selected)?;
        assert_eq!(persisted.get("args"), Some(&json!(["${CALLER_BINDING}"])));
        let output = serde_json::to_string(&persisted)?;
        assert!(!output.contains("resolved-private") && !output.contains("ambient-private"));
        Ok(())
    }
}
