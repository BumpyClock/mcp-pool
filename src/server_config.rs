use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ConfigurationEntry, ServerDef};

#[path = "server_config_values.rs"]
mod values;

#[cfg(test)]
#[path = "configuration_reader_tests.rs"]
mod tests;

pub struct ServerConfiguration {
    pub source: PathBuf,
    pub servers: BTreeMap<String, ConfiguredServer>,
    /// CLI callers must surface these diagnostics rather than silently omit imports.
    pub warnings: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ConfiguredServer {
    pub name: String,
    pub definition: ServerDef,
    /// Original entry for OAuth metadata. May contain secrets; do not log this value.
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
    #[serde(rename = "bearerToken", alias = "bearer_token")]
    bearer_token: Option<String>,
    #[serde(rename = "bearerTokenEnv", alias = "bearer_token_env")]
    bearer_token_env: Option<String>,
}

/// Selects an explicit path, then MCPORTER_CONFIG, then ~/.mcporter/mcporter.json.
/// Reads only that file's entries; editor imports and migration are deferred.
/// MCP_POOL_HOME does not change this selection.
pub fn load(path: Option<PathBuf>) -> Result<ServerConfiguration> {
    let selected = match path {
        Some(path) => path,
        None => match std::env::var("MCPORTER_CONFIG") {
            Ok(value) if !value.trim().is_empty() => PathBuf::from(value.trim()),
            Ok(_) | Err(std::env::VarError::NotPresent) => dirs::home_dir()
                .ok_or_else(|| anyhow!("home directory not found"))?
                .join(".mcporter")
                .join("mcporter.json"),
            Err(std::env::VarError::NotUnicode(_)) => bail!("MCPORTER_CONFIG is not valid Unicode"),
        },
    };
    let source = absolute_source(&selected)?;
    let contents = std::fs::read_to_string(&source)
        .with_context(|| format!("could not read mcporter config {}", source.display()))?;
    parse_config(&source, &contents)
}

/// Parses JSONC without connecting to servers or changing user files.
/// Relative command paths and cwd resolve from source; absent cwd uses its directory.
/// Stdio definitions carry the caller's environment, overlaid by configured values.
pub fn parse_config(source: &Path, contents: &str) -> Result<ServerConfiguration> {
    parse_with_environment(
        source,
        contents,
        &|name| match std::env::var(name) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                bail!("environment variable '{name}' is not valid Unicode")
            }
        },
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

fn parse_with_environment(
    source: &Path,
    contents: &str,
    environment: &impl Fn(&str) -> Result<Option<String>>,
    caller_environment: &impl Fn() -> Result<BTreeMap<String, String>>,
) -> Result<ServerConfiguration> {
    let source = absolute_source(source)?;
    let options = jsonc_parser::ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
    };
    // Parser and deserializer errors can contain config values, including credentials.
    let raw = jsonc_parser::parse_to_serde_value(contents.trim_start_matches('\u{feff}'), &options)
        .map_err(|_| anyhow!("invalid JSONC in mcporter config {}", source.display()))?
        .ok_or_else(|| anyhow!("empty mcporter config {}", source.display()))?;
    let object = raw
        .as_object()
        .ok_or_else(|| anyhow!("mcporter config {} must be an object", source.display()))?;
    let entries = object
        .get("mcpServers")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            anyhow!(
                "mcporter config {} needs an mcpServers object",
                source.display()
            )
        })?;
    let mut warnings = Vec::new();
    match object.get("imports") {
        Some(Value::Array(imports)) => {
            if imports.iter().any(|entry| !entry.is_string()) {
                bail!(
                    "mcporter config {} imports must be an array of strings",
                    source.display()
                );
            }
            if !imports.is_empty() {
                warnings.push(format!(
                    "Editor config imports are deferred: ignoring {} import(s) in {}; only this file's mcpServers are loaded.",
                    imports.len(),
                    source.display()
                ));
            }
        }
        Some(_) => bail!(
            "mcporter config {} imports must be an array",
            source.display()
        ),
        None => warnings.push(format!(
            "Automatic editor config imports are deferred; only mcpServers from {} are loaded.",
            source.display()
        )),
    }
    let directory = source
        .parent()
        .ok_or_else(|| anyhow!("mcporter config has no parent directory"))?;
    let mut servers = BTreeMap::new();
    for (name, raw) in entries {
        let entry: Entry = serde_json::from_value(raw.clone()).map_err(|_| {
            anyhow!(
                "invalid server definition for '{name}' in {}: expected string fields, string arrays/maps, and an unsigned timeoutMs",
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
        warnings,
    })
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
        }),
        description: expand(entry.description.as_deref().unwrap_or_default())
            .context("field 'description'")?,
        timeout_ms: entry.timeout_ms,
        ..ServerDef::default()
    };
    for (name, value) in entry.env {
        definition
            .env
            .insert(name, expand(&value).context("field 'env'")?);
    }
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
    let transport =
        expand(entry.transport.as_deref().unwrap_or_default()).context("field 'transport'")?;
    let url = entry
        .base_url
        .or(entry.snake_base_url)
        .or(entry.url)
        .or(entry.server_url)
        .or(entry.snake_server_url);
    if let Some(url) = url.filter(|url| !url.is_empty()) {
        definition.url = expand(&url).context("field 'url'")?;
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
            if !entry.args.is_empty() || values::existing_command_path(&command, directory)? {
                let mut tokens = vec![command];
                for argument in entry.args {
                    tokens.push(expand(&argument).context("field 'args'")?);
                }
                tokens
            } else {
                values::command_tokens(&command).context("field 'command'")?
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
            .any(|configured| name.eq_ignore_ascii_case(configured))
    });
    definition.env.extend(configured_environment);
    definition.clear_env = true;
    Ok(definition)
}
