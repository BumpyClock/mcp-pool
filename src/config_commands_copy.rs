use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

#[allow(clippy::too_many_arguments)]
pub(crate) fn apply(
    entry: &mut Value,
    copy_from: &str,
    from: Option<PathBuf>,
    root: Option<&Path>,
    command: Option<String>,
    url: Option<String>,
    transport: Option<String>,
    passed: Vec<String>,
    environment: Map<String, Value>,
    headers: Map<String, Value>,
) -> Result<()> {
    let (source, original_name) = copy_from
        .split_once(':')
        .filter(|(source, original_name)| !source.is_empty() && !original_name.is_empty())
        .context("--copy-from requires SOURCE:NAME")?;
    if command.is_some() && url.is_some() {
        bail!("Specify either a URL or a stdio command");
    }
    let project = crate::config_paths::project_directory(root)?;
    let home = dirs::home_dir().context("Home directory unavailable")?;
    let from = from
        .as_deref()
        .map(crate::config_paths::absolute)
        .transpose()?;
    let batch = crate::config_import_sources::collect(
        source,
        from.as_deref(),
        &project,
        &home,
        &[original_name.to_owned()],
    )?;
    for warning in &batch.warnings {
        crate::mcp_cli::context::warning(warning);
    }
    let overrides = std::mem::replace(entry, json!({}));
    *entry = batch
        .servers
        .get(original_name)
        .with_context(|| format!("Import source has no server named '{original_name}'"))?
        .entry
        .clone();
    let object = entry
        .as_object_mut()
        .context("Imported server definition must be an object")?;
    object.extend(
        overrides
            .as_object()
            .context("Add overrides must be an object")?
            .clone(),
    );
    let was_stdio = object.contains_key("command") || object.contains_key("executable");
    if url.is_some() || command.is_some() {
        for key in crate::server_config::URL_FIELDS.iter().copied().chain([
            "command",
            "executable",
            "args",
        ]) {
            object.remove(key);
        }
    }
    if let Some(url) = url {
        object.insert("baseUrl".to_owned(), json!(url));
        if was_stdio && transport.is_none() {
            object.insert("transport".to_owned(), json!("http"));
        }
    }
    if let Some(command) = command {
        object.insert("command".to_owned(), json!(command));
        object.insert("args".to_owned(), json!(passed));
        if !was_stdio && transport.is_none() {
            object.insert("transport".to_owned(), json!("stdio"));
        }
    } else if !passed.is_empty() {
        if let Some(tokens) = object.get("command").and_then(Value::as_array) {
            let executable = tokens
                .first()
                .context("Imported command has no executable")?;
            let mut tokens = vec![executable.clone()];
            tokens.extend(passed.into_iter().map(|argument| json!(argument)));
            object.insert("command".to_owned(), Value::Array(tokens));
            object.remove("args");
        } else if object.contains_key("command") || object.contains_key("executable") {
            object.insert("args".to_owned(), json!(passed));
        } else {
            bail!("--arg requires a stdio command");
        }
    }
    if let Some(transport) = transport {
        object.insert("transport".to_owned(), json!(transport));
    }
    for (key, values) in [("env", environment), ("headers", headers)] {
        if !values.is_empty() {
            object
                .entry(key)
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .with_context(|| format!("Imported {key} must be an object"))?
                .extend(values);
        }
    }
    Ok(())
}
