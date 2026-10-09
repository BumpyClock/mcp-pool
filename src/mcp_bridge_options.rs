use anyhow::{Result, bail};
use serde_json::Value;

use crate::server_config::{ConfiguredServer, ServerConfiguration};

pub(crate) const DEFAULT_HTTP_HOST: &str = "127.0.0.1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServeMode {
    Stdio,
    Http { host: String, port: u16 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServeOptions {
    pub mode: ServeMode,
    pub servers: Option<Vec<String>>,
}

pub(crate) fn parse(arguments: Vec<String>) -> Result<ServeOptions> {
    let mut host = None;
    let mut port = None;
    let mut servers = None;
    let mut explicit_stdio = false;
    let mut arguments = arguments.into_iter();

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--stdio" => {
                explicit_stdio = true;
            }
            "--http" => {
                let value = arguments
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("Flag '--http' requires a port."))?;
                port = Some(parse_port(&value)?);
            }
            "--host" => {
                host = Some(non_empty(
                    arguments.next().as_deref(),
                    "Flag '--host' requires a value.",
                )?);
            }
            "--servers" => {
                let value = arguments.next().ok_or_else(|| {
                    anyhow::anyhow!("Flag '--servers' requires a comma-separated list.")
                })?;
                servers = Some(parse_servers(&value)?);
            }
            _ if argument.starts_with("--http=") => {
                port = Some(parse_port(argument.trim_start_matches("--http="))?);
            }
            _ if argument.starts_with("--host=") => {
                host = Some(non_empty(
                    Some(argument.trim_start_matches("--host=")),
                    "Flag '--host' requires a value.",
                )?);
            }
            _ if argument.starts_with("--servers=") => {
                servers = Some(parse_servers(argument.trim_start_matches("--servers="))?);
            }
            _ => bail!("Unknown serve flag '{argument}'."),
        }
    }

    if explicit_stdio && port.is_some() {
        bail!("Flags '--stdio' and '--http' cannot be used together.");
    }
    if host.is_some() && port.is_none() {
        bail!("Flag '--host' can only be used with '--http'.");
    }
    let mode = match port {
        Some(port) => ServeMode::Http {
            host: host.unwrap_or_else(|| DEFAULT_HTTP_HOST.to_owned()),
            port,
        },
        None => ServeMode::Stdio,
    };

    Ok(ServeOptions { mode, servers })
}

pub(crate) fn select_servers(
    configuration: &ServerConfiguration,
    requested: Option<&[String]>,
) -> Result<Vec<ConfiguredServer>> {
    let mut selected = Vec::new();
    if let Some(requested) = requested {
        for name in requested {
            let server = configuration.servers.get(name).ok_or_else(|| {
                anyhow::anyhow!(
                    "Server '{name}' is not configured for keep-alive and cannot be served."
                )
            })?;
            if !is_keep_alive(server) {
                bail!("Server '{name}' is not configured for keep-alive and cannot be served.");
            }
            selected.push(server.clone());
        }
    } else {
        selected.extend(
            configuration
                .servers
                .values()
                .filter(|server| is_keep_alive(server))
                .cloned(),
        );
    }

    if selected.is_empty() {
        bail!("No MCP servers are configured for keep-alive; nothing to serve.");
    }
    Ok(selected)
}

fn parse_port(value: &str) -> Result<u16> {
    if value.trim().is_empty() {
        bail!("Flag '--http' requires a port.");
    }
    value
        .parse::<u16>()
        .map_err(|_| anyhow::anyhow!("Invalid HTTP port '{value}'."))
}

fn parse_servers(value: &str) -> Result<Vec<String>> {
    let names: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect();
    if names.is_empty() {
        bail!("Flag '--servers' requires at least one server name.");
    }
    for (position, name) in names.iter().enumerate() {
        if names.iter().take(position).any(|previous| previous == name) {
            bail!("Flag '--servers' contains duplicate server '{name}'.");
        }
    }
    Ok(names)
}

fn non_empty(value: Option<&str>, error: &str) -> Result<String> {
    let value = value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("{error}"))?;
    Ok(value.to_owned())
}

fn is_keep_alive(server: &ConfiguredServer) -> bool {
    let canonical = default_keep_alive_name(server);
    let name = server.name.to_lowercase();
    let candidates = [Some(name.as_str()), canonical.as_deref()];
    let enabled = environment_override("MCPORTER_KEEPALIVE");
    let disabled = std::env::var("MCPORTER_DISABLE_KEEPALIVE")
        .ok()
        .or_else(|| std::env::var("MCPORTER_NO_KEEPALIVE").ok())
        .map(|value| OverrideSet::parse(&value))
        .unwrap_or_default();

    if enabled.matches(&candidates) {
        return true;
    }
    if disabled.matches(&candidates) {
        return false;
    }
    if let Some(configured) = configured_lifecycle(&server.raw) {
        return configured;
    }
    if std::iter::once(server.definition.command.as_str())
        .chain(server.definition.args.iter().map(String::as_str))
        .any(|argument| {
            argument.contains(r"\${CHROME_DEVTOOLS_URL}")
                || argument.contains("$env:CHROME_DEVTOOLS_URL")
        })
    {
        return false;
    }
    candidates.iter().flatten().any(|candidate| {
        matches!(
            *candidate,
            "chrome-devtools" | "mobile-mcp" | "playwright" | "cloudbase"
        )
    })
}

fn configured_lifecycle(raw: &Value) -> Option<bool> {
    let lifecycle = raw.get("lifecycle")?;
    match lifecycle {
        Value::String(value) if value == "keep-alive" => Some(true),
        Value::String(value) if value == "ephemeral" => Some(false),
        Value::Object(_) => match lifecycle.get("mode").and_then(Value::as_str) {
            Some("keep-alive") => Some(true),
            Some("ephemeral") => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn default_keep_alive_name(server: &ConfiguredServer) -> Option<String> {
    let fragments = [
        ("chrome-devtools", &["chrome-devtools-mcp"][..]),
        ("mobile-mcp", &["@mobilenext/mobile-mcp", "mobile-mcp"][..]),
        ("playwright", &["@playwright/mcp", "playwright/mcp"][..]),
        (
            "cloudbase",
            &["@cloudbase/cloudbase-mcp", "cloudbase-mcp"][..],
        ),
    ];
    let command_parts: Vec<&str> = std::iter::once(server.definition.command.as_str())
        .chain(server.definition.args.iter().map(String::as_str))
        .collect();
    for (label, needles) in fragments {
        if command_parts.iter().any(|part| {
            let part = part.to_lowercase();
            needles.iter().any(|needle| part.contains(needle))
        }) {
            return Some(label.to_owned());
        }
    }
    None
}

#[derive(Default)]
struct OverrideSet {
    all: bool,
    names: Vec<String>,
}

impl OverrideSet {
    fn parse(value: &str) -> Self {
        let names: Vec<String> = value
            .split(',')
            .map(|token| token.trim().to_lowercase())
            .filter(|token| !token.is_empty())
            .collect();
        Self {
            all: names.iter().any(|name| name == "*"),
            names,
        }
    }

    fn matches(&self, candidates: &[Option<&str>]) -> bool {
        self.all
            || candidates
                .iter()
                .flatten()
                .any(|candidate| self.names.iter().any(|name| name == candidate))
    }
}

fn environment_override(name: &str) -> OverrideSet {
    std::env::var(name)
        .ok()
        .map(|value| OverrideSet::parse(&value))
        .unwrap_or_default()
}
