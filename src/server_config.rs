use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;

use crate::config::{ConfigurationEntry, ServerDef};

#[path = "server_config_entry.rs"]
mod entry;
use entry::{Command, Entry, EntryPolicy};
pub(crate) use entry::{URL_FIELDS, is_remote_entry, is_server_entry};

#[path = "server_config_values.rs"]
mod values;
pub(crate) use values::{command_tokens, existing_command_path, resolve_path};

#[path = "server_config_settings.rs"]
mod settings;
pub(crate) use settings::{ConfiguredEnvironment, OAuthSettings};
#[path = "server_config_auth.rs"]
mod authentication;
pub(crate) use authentication::authentication_server;

#[cfg(test)]
#[path = "configuration_reader_tests.rs"]
mod tests;

pub struct ServerConfiguration {
    pub source: PathBuf,
    pub servers: BTreeMap<String, ConfiguredServer>,
    pub credential_scope: Option<PathBuf>,
    /// CLI callers must surface these diagnostics rather than silently omit imports.
    pub warnings: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ConfiguredServer {
    pub name: String,
    pub definition: ServerDef,
    /// Original entry for settings, persistence, and output. May contain secrets.
    pub raw: Value,
    pub source: PathBuf,
}

/// Preserves readable compatibility entries and defers environment resolution until use.
pub(crate) fn validate_document(document: &Value) -> Result<()> {
    validate_entries(document, EntryPolicy::Preserve)
}

pub(crate) fn validate_new_entries(document: &Value) -> Result<()> {
    validate_entries(document, EntryPolicy::New)
}

fn validate_entries(document: &Value, policy: EntryPolicy) -> Result<()> {
    legacy_imports(document)?;
    let servers = document
        .get("mcpServers")
        .and_then(Value::as_object)
        .context("MCP config needs an mcpServers object")?;
    for (name, raw) in servers {
        if name.trim().is_empty() {
            bail!("Server name cannot be empty");
        }
        Entry::decode(raw)
            .and_then(|entry| entry.validate(policy))
            .with_context(|| format!("Invalid definition for server '{name}'"))?;
        crate::tool_filter::ToolFilter::from_raw(raw)?;
    }
    Ok(())
}

fn legacy_imports(document: &Value) -> Result<&[Value]> {
    match document.get("imports") {
        None => Ok(&[]),
        Some(Value::Array(imports)) if imports.iter().all(Value::is_string) => Ok(imports),
        Some(_) => bail!("imports must be an array of strings"),
    }
}

pub(crate) fn load_at(path: Option<PathBuf>, root: Option<&Path>) -> Result<ServerConfiguration> {
    if let Some(source) = crate::config_paths::explicit(path)? {
        return parse_document(&source, &read_raw(&source, false)?);
    }
    let home = crate::config_paths::absolute(&crate::config_paths::home_config()?)?;
    let project = crate::config_paths::absolute(&crate::config_paths::project_config(root)?)?;
    let mut home_document = read_raw(&home, true)?;
    let project_document = if project != home {
        Some(read_raw(&project, true)?)
    } else {
        None
    };
    if let Some(local) = &project_document {
        let local = local
            .get("mcpServers")
            .and_then(Value::as_object)
            .context("Project config needs an mcpServers object")?;
        let inherited = home_document
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .context("Home config needs an mcpServers object")?;
        inherited.retain(|name, _| !local.contains_key(name));
    }
    let mut configuration = parse_document(&home, &home_document)?;
    if project != home {
        let local = parse_document(
            &project,
            &project_document.context("Project config missing")?,
        )?;
        configuration.servers.extend(local.servers);
        configuration.warnings.extend(local.warnings);
        if project.is_file() {
            configuration.source = project;
            configuration.credential_scope = local.credential_scope;
        }
    }
    Ok(configuration)
}

fn read_raw(source: &Path, optional: bool) -> Result<Value> {
    let contents = match std::fs::read_to_string(source) {
        Ok(contents) => contents,
        Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({"mcpServers":{}}));
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Could not read MCP config {}", source.display()));
        }
    };
    parse_json(source, &contents)
}

/// Parses JSONC without connecting to servers or changing user files.
/// Relative command paths and cwd resolve from source; absent cwd uses its directory.
/// Stdio definitions carry the caller's environment, overlaid by configured values.
#[cfg(test)]
pub fn parse_config(source: &Path, contents: &str) -> Result<ServerConfiguration> {
    parse_with_environment(
        source,
        contents,
        &values::environment_variable,
        &values::caller_environment,
    )
}

pub(crate) fn parse_document(source: &Path, document: &Value) -> Result<ServerConfiguration> {
    normalize_document(
        absolute_source(source)?,
        document,
        &values::environment_variable,
        &values::caller_environment,
    )
}

fn absolute_source(source: &Path) -> Result<PathBuf> {
    let expanded = values::expand_home(source)?;
    if expanded.is_absolute() || values::windows_absolute(&expanded.to_string_lossy()) {
        Ok(expanded)
    } else {
        Ok(std::env::current_dir()?.join(expanded))
    }
}

/// Omits parser details from errors because they can contain credentials.
#[cfg(test)]
fn parse_with_environment(
    source: &Path,
    contents: &str,
    environment: &impl Fn(&str) -> Result<Option<String>>,
    caller_environment: &impl Fn() -> Result<BTreeMap<String, String>>,
) -> Result<ServerConfiguration> {
    let source = absolute_source(source)?;
    let raw = parse_json(&source, contents)?;
    normalize_document(source, &raw, environment, caller_environment)
}

fn normalize_document(
    source: PathBuf,
    raw: &Value,
    environment: &impl Fn(&str) -> Result<Option<String>>,
    caller_environment: &impl Fn() -> Result<BTreeMap<String, String>>,
) -> Result<ServerConfiguration> {
    let object = raw
        .as_object()
        .ok_or_else(|| anyhow!("MCP config {} must be an object", source.display()))?;
    let entries = object
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("MCP config {} needs an mcpServers object", source.display()))?;
    let mut warnings = Vec::new();
    let imports =
        legacy_imports(raw).map_err(|error| anyhow!("MCP config {} {error}", source.display()))?;
    if !imports.is_empty() {
        warnings.push(format!(
                    "Ignoring {} legacy import(s) in {}; run `mcp-cli import SOURCE` to copy server definitions explicitly.",
                    imports.len(),
                    source.display()
                ));
    }
    let directory = source
        .parent()
        .ok_or_else(|| anyhow!("MCP config has no parent directory"))?
        .to_path_buf();
    let mut servers = BTreeMap::new();
    for (name, raw) in entries {
        let entry = Entry::decode(raw).map_err(|_| {
            anyhow!(
                "invalid server definition for '{name}' in {}: expected string fields, string arrays/maps, an unsigned timeoutMs, and protocolVersion auto, legacy, or 2026-07-28",
                source.display()
            )
        })?;
        let definition = normalize(
            name,
            &source,
            entry,
            &directory,
            environment,
            caller_environment,
        )
        .with_context(|| format!("server '{name}' in {}", source.display()))?;
        servers.insert(
            name.clone(),
            ConfiguredServer {
                name: name.clone(),
                definition,
                raw: raw.clone(),
                source: source.clone(),
            },
        );
    }
    Ok(ServerConfiguration {
        source,
        servers,
        credential_scope: Some(directory),
        warnings,
    })
}

fn parse_json(source: &Path, contents: &str) -> Result<Value> {
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    };
    jsonc_parser::parse_to_serde_value(contents.trim_start_matches('\u{feff}'), &options)
        .map_err(|_| anyhow!("invalid JSONC in MCP config {}", source.display()))?
        .ok_or_else(|| anyhow!("empty MCP config {}", source.display()))
}

fn normalize(
    name: &str,
    source: &Path,
    entry: Entry,
    directory: &Path,
    environment: &impl Fn(&str) -> Result<Option<String>>,
    caller_environment: &impl Fn() -> Result<BTreeMap<String, String>>,
) -> Result<ServerDef> {
    let expand = |value: &str| values::expand_environment(value, environment);
    let transport =
        expand(entry.transport.as_deref().unwrap_or_default()).context("field 'transport'")?;
    let transport = entry.transport(&transport)?;
    let url = entry.remote_url().map(str::to_owned);

    let mut definition = ServerDef {
        credential_scope: Some(directory.to_owned()),
        configuration_entry: Some(ConfigurationEntry {
            source: source.to_owned(),
            name: name.to_owned(),
            scope: Some(directory.to_owned()),
            credential_store: None,
        }),
        description: expand(entry.description.as_deref().unwrap_or_default())
            .context("field 'description'")?,
        timeout_ms: entry.timeout_ms,
        protocol_version: entry.protocol_version,
        ..ServerDef::default()
    };
    for (name, value) in entry.env {
        definition
            .env
            .insert(name, expand(&value).context("field 'env'")?);
    }
    let effective_environment = |name: &str| {
        definition
            .env
            .iter()
            .find(|(configured, _)| settings::environment_names_match(configured, name))
            .map_or_else(|| environment(name), |(_, value)| Ok(Some(value.clone())))
    };
    {
        let expand = |value: &str| values::expand_environment(value, &effective_environment);
        for (name, value) in entry.headers {
            definition
                .headers
                .insert(name, expand(&value).context("field 'headers'")?);
        }
        if let Some(token) = entry.bearer_token {
            definition.headers.insert(
                "Authorization".into(),
                format!("Bearer {}", expand(&token).context("field 'bearerToken'")?),
            );
        }
        if let Some(name) = entry.bearer_token_env {
            let token = expand(&format!("$env:{name}")).context("field 'bearerTokenEnv'")?;
            definition.headers.insert("Authorization".into(), token);
        }
    }
    definition.transport = transport.to_owned();
    if let Some(url) = url {
        definition.url =
            values::expand_environment(&url, &effective_environment).context("field 'url'")?;
        entry::validate_url(&definition.url)?;
        return Ok(definition);
    }
    let command = entry
        .command
        .or_else(|| entry.executable.map(Command::Text));
    let tokens = match command {
        Some(Command::Tokens(tokens)) => tokens
            .iter()
            .map(|value| expand(value).context("field 'command'"))
            .collect::<Result<Vec<_>>>()?,
        Some(Command::Text(command)) => {
            let command = expand(&command).context("field 'command'")?;
            if !entry.args.is_empty() || existing_command_path(&command, directory)? {
                let mut tokens = vec![command];
                for argument in entry.args {
                    tokens.push(expand(&argument).context("field 'args'")?);
                }
                tokens
            } else {
                command_tokens(&command).context("field 'command'")?
            }
        }
        None => bail!("missing baseUrl/url or command definition"),
    };
    let mut tokens = tokens.into_iter();
    let command = tokens
        .next()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow!("field 'command' must contain an executable"))?;
    definition.command = if values::looks_like_path(&command) {
        values::resolve_path(Path::new(&command), directory)?
            .to_string_lossy()
            .into_owned()
    } else {
        command
    };
    definition.args = tokens.collect();
    let cwd = entry
        .cwd
        .map(|value| expand(&value).context("field 'cwd'"))
        .transpose()?;
    definition.cwd = Some(match cwd.filter(|value| !value.is_empty()) {
        Some(cwd) => values::resolve_path(Path::new(&cwd), directory)?,
        None => directory.to_path_buf(),
    });
    let configured_environment = std::mem::take(&mut definition.env);
    definition.env = caller_environment().context("could not capture caller environment")?;
    #[cfg(windows)]
    definition.env.retain(|name, _| {
        !configured_environment
            .keys()
            .any(|configured| settings::environment_names_match(configured, name))
    });
    definition.env.extend(configured_environment);
    definition.clear_env = true;
    Ok(definition)
}
