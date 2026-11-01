use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::json;

use crate::config::{ConfigurationEntry, ServerDef};

pub(crate) const URL_FIELDS: &[&str] = &["baseUrl", "base_url", "url", "serverUrl", "server_url"];

pub(crate) fn is_remote_entry(entry: &serde_json::Map<String, Value>) -> bool {
    URL_FIELDS.iter().any(|field| entry.contains_key(*field))
}

pub(crate) fn is_server_entry(entry: &serde_json::Map<String, Value>) -> bool {
    is_remote_entry(entry) || entry.contains_key("command") || entry.contains_key("executable")
}

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

#[derive(Deserialize)]
#[serde(untagged)]
enum Command {
    Text(String),
    Tokens(Vec<String>),
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Entry {
    url: Option<String>,
    #[serde(rename = "baseUrl")]
    base_url: Option<String>,
    #[serde(rename = "base_url")]
    snake_base_url: Option<String>,
    #[serde(rename = "serverUrl")]
    server_url: Option<String>,
    #[serde(rename = "server_url")]
    snake_server_url: Option<String>,
    command: Option<Command>,
    executable: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    headers: BTreeMap<String, String>,
    cwd: Option<String>,
    transport: Option<String>,
    description: Option<String>,
    #[serde(rename = "timeoutMs", alias = "timeout_ms")]
    timeout_ms: Option<u64>,
    #[serde(rename = "protocolVersion", alias = "protocol_version")]
    protocol_version: crate::mcp_protocol::ProtocolPreference,
    #[serde(rename = "bearerToken", alias = "bearer_token")]
    bearer_token: Option<String>,
    #[serde(rename = "bearerTokenEnv", alias = "bearer_token_env")]
    bearer_token_env: Option<String>,
}

pub(crate) fn validate_document(document: &Value) -> Result<()> {
    legacy_imports(document)?;
    let servers = document
        .get("mcpServers")
        .and_then(Value::as_object)
        .context("MCP config needs an mcpServers object")?;
    for (name, raw) in servers {
        if name.trim().is_empty() {
            bail!("Server name cannot be empty");
        }
        let entry: Entry = serde_json::from_value(raw.clone())
            .map_err(|_| anyhow!("Invalid field types for server '{name}'"))?;
        let remote = entry
            .url
            .as_ref()
            .or(entry.base_url.as_ref())
            .or(entry.snake_base_url.as_ref())
            .or(entry.server_url.as_ref())
            .or(entry.snake_server_url.as_ref());
        let command = entry.command.is_some() || entry.executable.is_some();
        if remote.is_some() == command {
            bail!("Server '{name}' needs exactly one URL or command");
        }
        let executable = match &entry.command {
            Some(Command::Text(command)) => Some(command.as_str()),
            Some(Command::Tokens(tokens)) => tokens.first().map(String::as_str),
            None => entry.executable.as_deref(),
        };
        if command && executable.is_none_or(|value| value.trim().is_empty()) {
            bail!("Server '{name}' needs a nonempty executable");
        }
        let transport =
            entry
                .transport
                .as_deref()
                .unwrap_or(if remote.is_some() { "http" } else { "stdio" });
        if !matches!(transport, "http" | "streamable-http" | "sse" | "stdio")
            || (transport == "stdio") != command
        {
            bail!("Server '{name}' transport must agree with its URL or command");
        }
        if let Some(url) = remote
            && !url.contains('$')
        {
            let url = reqwest::Url::parse(url)
                .map_err(|_| anyhow!("Server '{name}' needs an absolute HTTP URL"))?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                bail!("Server '{name}' needs an absolute HTTP URL");
            }
        }
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
        let mut configuration = parse_document(&source, &read_raw(&source, false)?)?;
        set_credential_scope(
            &mut configuration,
            source.parent().context("Config has no parent")?,
        );
        return Ok(configuration);
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
            configuration.source = project.clone();
        }
    }
    set_credential_scope(
        &mut configuration,
        project.parent().context("Config has no parent")?,
    );
    Ok(configuration)
}

fn set_credential_scope(configuration: &mut ServerConfiguration, directory: &Path) {
    configuration.credential_scope = Some(directory.to_path_buf());
    for server in configuration.servers.values_mut() {
        server.definition.credential_scope = Some(directory.to_path_buf());
        if let Some(entry) = server.definition.configuration_entry.as_mut() {
            entry.scope = Some(directory.to_path_buf());
        }
    }
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
                    "Ignoring {} legacy import(s) in {}; run `mcp-pool import SOURCE` to copy server definitions explicitly.",
                    imports.len(),
                    source.display()
                ));
    }
    let directory = source
        .parent()
        .ok_or_else(|| anyhow!("MCP config has no parent directory"))?;
    let mut servers = BTreeMap::new();
    for (name, raw) in entries {
        let entry: Entry = serde_json::from_value(raw.clone()).map_err(|_| {
            anyhow!(
                "invalid server definition for '{name}' in {}: expected string fields, string arrays/maps, an unsigned timeoutMs, and protocolVersion auto, legacy, or 2026-07-28",
                source.display()
            )
        })?;
        let definition = normalize(
            name,
            &source,
            entry,
            directory,
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
        credential_scope: None,
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
    let mut definition = ServerDef {
        configuration_entry: Some(ConfigurationEntry {
            source: source.to_owned(),
            name: name.to_owned(),
            scope: None,
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
    let transport =
        expand(entry.transport.as_deref().unwrap_or_default()).context("field 'transport'")?;
    let url = entry
        .base_url
        .or(entry.snake_base_url)
        .or(entry.url)
        .or(entry.server_url)
        .or(entry.snake_server_url);
    if let Some(url) = url.filter(|url| !url.is_empty()) {
        definition.url =
            values::expand_environment(&url, &effective_environment).context("field 'url'")?;
        let parsed = reqwest::Url::parse(&definition.url)
            .map_err(|_| anyhow!("field 'url' must be an absolute HTTP/SSE URL"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            bail!("field 'url' must be an absolute HTTP/SSE URL");
        }
        definition.transport = match transport.as_str() {
            "" | "http" | "streamable-http" => "http".into(),
            "sse" => "sse".into(),
            _ => bail!("field 'transport' must be http or sse for a URL"),
        };
        return Ok(definition);
    }
    if !matches!(transport.as_str(), "" | "stdio") {
        bail!("field 'transport' must be stdio for a command");
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
