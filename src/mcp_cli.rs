use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::config::ServerDef;
use crate::mcp_client::McpClient;
use crate::server_config::{ConfiguredServer, ServerConfiguration};
use crate::tool_arguments::{self, AdHoc, Output};
use crate::{config_commands, daemon_commands, tool_discovery, tool_output};

#[path = "cli_context.rs"]
pub(crate) mod context;
#[path = "pool_identity.rs"]
mod identity;
#[path = "server_selector.rs"]
pub(crate) mod selector;
#[path = "stdio_tokens.rs"]
mod stdio;
use stdio::command_tokens;
#[path = "output_artifacts.rs"]
mod artifacts;
#[path = "adhoc_persistence.rs"]
mod persistence;
pub(crate) use persistence::persist as persist_ad_hoc;
#[path = "cli_help.rs"]
mod help;
#[path = "tool_call.rs"]
mod invocation;
pub(crate) use help::command_help;

const COMMANDS: &[&str] = &[
    "list",
    "describe",
    "list-tools",
    "call",
    "auth",
    "vault",
    "resource",
    "resources",
    "config",
    "daemon",
    "serve",
];

pub fn handles(arguments: &[String]) -> bool {
    context::command_index(arguments)
        .and_then(|index| arguments.get(index))
        .is_some_and(|command| COMMANDS.contains(&command.as_str()))
}

pub(crate) fn command_position(arguments: &[String]) -> Option<usize> {
    context::command_index(arguments)
}

pub async fn run(arguments: Vec<String>) -> Result<()> {
    let context::ContextOptions {
        path,
        command,
        arguments,
        oauth_timeout,
    } = context::parse(arguments)?;
    if oauth_timeout.is_some()
        && command != "auth"
        && !(command == "config"
            && arguments
                .first()
                .is_some_and(|argument| argument == "login"))
    {
        bail!(
            "--oauth-timeout applies only to auth or config login. Call/list cached-token refresh uses its fixed OAuth deadline; use --timeout for the operation wait budget."
        );
    }
    if arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "--help" | "-h"))
        && command != "serve"
    {
        command_help(&command);
        return Ok(());
    }
    if command == "config" {
        return config_commands::run_with_timeout(path, arguments, oauth_timeout).await;
    }
    if command == "daemon" {
        return daemon_commands::daemon(arguments).await;
    }
    let configuration = load(path)?;
    match command.as_str() {
        "list" | "describe" | "list-tools" => tool_discovery::run(configuration, arguments).await,
        "call" => invocation::run(&configuration, arguments).await,
        "resource" | "resources" => resource(&configuration, arguments).await,
        "auth" => {
            daemon_commands::auth_with_timeout(&configuration, arguments, oauth_timeout).await
        }
        "vault" => daemon_commands::vault(&configuration, arguments).await,
        "serve" => crate::mcp_bridge::run(configuration, arguments).await,
        _ => bail!("Unknown MCP command '{command}'"),
    }
}

pub(crate) fn load(path: Option<PathBuf>) -> Result<ServerConfiguration> {
    let configuration = crate::server_config::load(path)?;
    for warning in &configuration.warnings {
        context::warning(warning);
    }
    Ok(configuration)
}

pub fn pool_name(server: &ConfiguredServer, definition: &ServerDef) -> Result<String> {
    identity::pool_name(server, definition)
}

pub(crate) fn server(
    configuration: &ServerConfiguration,
    name: &str,
    ephemeral: &AdHoc,
) -> Result<ConfiguredServer> {
    if !ephemeral.present() {
        if name.starts_with("https://") || name.starts_with("http://") {
            let mut ephemeral = ephemeral.clone();
            ephemeral.url = Some(name.to_owned());
            return server(configuration, name, &ephemeral);
        }
        ephemeral.validate()?;
        return configuration
            .servers
            .get(name)
            .cloned()
            .with_context(|| format!("Unknown MCP server '{name}'"));
    }
    if ephemeral.url.is_some() && ephemeral.command.is_some() {
        bail!("Choose either --http-url or --stdio, not both");
    }
    ephemeral.validate()?;
    let label = ephemeral.name.as_deref().unwrap_or_else(|| {
        if name.starts_with("https://") || name.starts_with("http://") {
            "adhoc"
        } else {
            name
        }
    });
    if reqwest::Url::parse(label).is_ok() {
        bail!("Ad-hoc --name must be a semantic alias, not a URL");
    }
    let mut entry = json!({"env":ephemeral.environment,"headers":ephemeral.headers});
    if let Some(object) = entry.as_object_mut() {
        if let Some(url) = &ephemeral.url {
            let parsed = reqwest::Url::parse(url).context("Invalid ad-hoc HTTP URL")?;
            if parsed.scheme() != "https" && !(parsed.scheme() == "http" && ephemeral.allow_http) {
                bail!("Ad-hoc URLs require HTTPS; use --allow-http to explicitly allow HTTP");
            }
            if !parsed.username().is_empty() || parsed.password().is_some() {
                bail!("Credentials in ad-hoc URL userinfo are not supported; use --header");
            }
            object.insert("baseUrl".to_owned(), json!(url));
            if let Some(transport) = &ephemeral.transport {
                object.insert("transport".to_owned(), json!(transport));
            }
        }
        if let Some(command) = &ephemeral.command {
            let mut tokens = command_tokens(command)?;
            tokens.extend(ephemeral.arguments.clone());
            object.insert("command".to_owned(), json!(tokens));
        }
        if let Some(cwd) = &ephemeral.cwd {
            object.insert("cwd".to_owned(), json!(cwd));
        }
        if let Some(description) = &ephemeral.description {
            object.insert("description".to_owned(), json!(description));
        }
    }
    let source = std::env::current_dir()?.join(".mcporter-adhoc.json");
    let configuration = crate::server_config::parse_config(
        &source,
        &serde_json::to_string(&json!({
            "mcpServers":{label:entry},"imports":[]
        }))?,
    )?;
    let mut selected = configuration
        .servers
        .get(label)
        .cloned()
        .context("Ad-hoc definition was not created")?;
    if let Some(path) = &ephemeral.persist {
        selected.source = persistence::destination(path)?;
    }
    selected.definition.configuration_entry = Some(crate::config::ConfigurationEntry {
        source: selected.source.clone(),
        name: selected.name.clone(),
    });
    Ok(selected)
}

pub(crate) async fn connect(
    server: &ConfiguredServer,
    timeout: Option<u64>,
    no_oauth: bool,
) -> Result<McpClient> {
    let definition = crate::oauth::prepare(server, no_oauth).await?;
    let name = pool_name(server, &definition)?;
    let environment = std::env::var("MCPORTER_CALL_TIMEOUT").ok();
    let timeout = context::timeout(
        timeout,
        environment.as_deref(),
        definition.timeout_ms,
        60_000,
    );
    McpClient::connect(&name, &definition, Duration::from_millis(timeout)).await
}

async fn resource(configuration: &ServerConfiguration, arguments: Vec<String>) -> Result<()> {
    let mut tokens = std::collections::VecDeque::from(arguments);
    let mut output = Output::Auto;
    let mut no_oauth = false;
    let mut timeout = None;
    let mut positional = Vec::new();
    while let Some(token) = tokens.pop_front() {
        match token.as_str() {
            "--output" => output = Output::parse(&tool_arguments::value(&mut tokens, &token)?)?,
            "--raw" => output = Output::Raw,
            "--json" => output = Output::Json,
            "--no-oauth" => no_oauth = true,
            "--timeout" => {
                timeout = Some(tool_arguments::positive_milliseconds(
                    &tool_arguments::value(&mut tokens, &token)?,
                )?)
            }
            _ if token.starts_with("--") => bail!("Unknown resource argument '{token}'"),
            _ => positional.push(token),
        }
    }
    if positional.is_empty() || positional.len() > 2 {
        bail!("Usage: resource SERVER [URI] [--output FORMAT]");
    }
    let name = positional.first().context("Missing server")?;
    let selected = server(configuration, name, &AdHoc::default())?;
    let outcome = async {
        let mut client = connect(&selected, timeout, no_oauth).await?;
        if let Some(uri) = positional.get(1) {
            client.request("resources/read", json!({"uri":uri})).await
        } else {
            client
                .list_resources()
                .await
                .map(|resources| json!({"resources":resources}))
        }
    }
    .await;
    print_outcome(&selected, None, outcome, output)
}

pub(crate) fn print_outcome(
    server: &ConfiguredServer,
    tool: Option<&str>,
    outcome: Result<Value>,
    output: Output,
) -> Result<()> {
    match outcome {
        Ok(result) => {
            println!("{}", tool_output::render(&result, output)?);
            if tool_output::failed(&result) {
                bail!("MCP operation returned an error result");
            }
            Ok(())
        }
        Err(error) => {
            let message = safe_error(server, &error);
            if matches!(output, Output::Json | Output::Raw) {
                let mut envelope = json!({"server":server.name,"error":message,"issue":tool_discovery::issue(&message)});
                if let Some(tool) = tool
                    && let Some(object) = envelope.as_object_mut()
                {
                    object.insert("tool".to_owned(), json!(tool));
                }
                println!("{}", serde_json::to_string_pretty(&envelope)?);
                bail!("MCP operation failed (details rendered above)");
            }
            bail!("{message}")
        }
    }
}

pub(crate) fn safe_error(server: &ConfiguredServer, error: &anyhow::Error) -> String {
    let mut message = format!("{error:#}");
    let configured = server.raw.get("env").and_then(Value::as_object);
    let environment = server
        .definition
        .env
        .iter()
        .filter(|(name, value)| {
            let sensitive_name = [
                "TOKEN",
                "SECRET",
                "PASSWORD",
                "CREDENTIAL",
                "API_KEY",
                "ACCESS_KEY",
            ]
            .iter()
            .any(|pattern| name.to_ascii_uppercase().contains(pattern));
            sensitive_name
                || (value.len() >= 4 && configured.is_some_and(|values| values.contains_key(*name)))
        })
        .map(|(_, value)| value);
    for secret in server.definition.headers.values().chain(environment) {
        if !secret.is_empty() {
            message = message.replace(secret, "[redacted]");
        }
    }
    if let Ok(url) = reqwest::Url::parse(&server.definition.url) {
        let sanitized = display_url(&server.definition.url);
        message = message.replace(url.as_str(), &sanitized);
        message = message.replace(&server.definition.url, &sanitized);
    }
    message
}

pub(crate) fn display_url(address: &str) -> String {
    let Ok(mut url) = reqwest::Url::parse(address) else {
        return "[configured endpoint]".to_owned();
    };
    if url.set_username("").is_err() || url.set_password(None).is_err() {
        return "[configured endpoint]".to_owned();
    }
    url.set_query(None);
    url.set_fragment(None);
    url.to_string()
}

pub(crate) async fn stdin() -> Result<String> {
    let mut contents = String::new();
    tokio::io::stdin()
        .take(16 * 1024 * 1024 + 1)
        .read_to_string(&mut contents)
        .await?;
    if contents.len() > 16 * 1024 * 1024 {
        bail!("JSON input exceeds 16 MiB");
    }
    Ok(contents)
}

pub(crate) fn tool_allowed(server: &ConfiguredServer, tool: &str) -> Result<bool> {
    Ok(crate::tool_filter::ToolFilter::from_raw(&server.raw)
        .with_context(|| format!("Server '{}' has invalid tool filters", server.name))?
        .permits(tool))
}

#[cfg(test)]
#[path = "cli_regression_tests.rs"]
mod regression_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routing_preserves_native_commands() {
        assert!(handles(&[
            "--config".into(),
            "fixture.json".into(),
            "list".into()
        ]));
        assert!(!handles(&["proxy".into(), "example".into()]));
        assert!(!handles(&["start".into()]));
        assert!(!handles(&["pool".into(), "list".into()]));
        assert!(!handles(&["generate-cli".into()]));
    }

    #[test]
    fn identities_separate_resolved_config_views_without_exposing_values() -> Result<()> {
        let source = PathBuf::from("synthetic.json");
        let first = crate::server_config::parse_config(
            &source,
            r#"{"mcpServers":{"docs":{"command":"echo","env":{"SECRET":"private"}}},"imports":[]}"#,
        )?;
        let server = first.servers.get("docs").context("docs")?;
        let name = pool_name(server, &server.definition)?;
        assert!(name.starts_with("mcp-docs-"));
        assert!(!name.contains("private"));
        assert_eq!(name, pool_name(server, &server.definition)?);
        let mut other = server.clone();
        other.source = PathBuf::from("other.json");
        assert_ne!(name, pool_name(&other, &other.definition)?);
        Ok(())
    }

    #[test]
    fn tool_policy_is_exact_and_rejects_ambiguous_filters() -> Result<()> {
        let source = PathBuf::from("synthetic.json");
        let configuration = crate::server_config::parse_config(
            &source,
            r#"{"mcpServers":{"docs":{"command":"echo","allowedTools":["search"]}},"imports":[]}"#,
        )?;
        let mut server = configuration.servers.get("docs").context("docs")?.clone();
        assert!(tool_allowed(&server, "search")?);
        assert!(!tool_allowed(&server, "search-other")?);
        if let Some(object) = server.raw.as_object_mut() {
            object.insert("blockedTools".to_owned(), json!([]));
        }
        assert!(tool_allowed(&server, "search").is_err());
        assert_eq!(
            display_url("https://user:secret@example.test/mcp?token=secret#private"),
            "https://example.test/mcp"
        );
        Ok(())
    }

    #[test]
    fn ad_hoc_stdio_tokens_and_appended_arguments_reach_the_shared_normalizer() -> Result<()> {
        let configuration = crate::server_config::parse_config(
            &PathBuf::from("synthetic.json"),
            r#"{"imports":[],"mcpServers":{}}"#,
        )?;
        let selected = server(
            &configuration,
            "adhoc",
            &AdHoc {
                command: Some("node \"server script.js\"".to_owned()),
                arguments: vec!["--extra".to_owned(), "value".to_owned()],
                ..AdHoc::default()
            },
        )?;
        assert_eq!(selected.definition.command, "node");
        assert_eq!(
            selected.definition.args,
            vec!["server script.js", "--extra", "value"]
        );
        assert_eq!(
            selected
                .definition
                .configuration_entry
                .as_ref()
                .map(|entry| entry.name.as_str()),
            Some("adhoc")
        );
        let named = server(
            &configuration,
            "http://example.test/mcp",
            &AdHoc {
                name: Some("explicit".to_owned()),
                allow_http: true,
                ..AdHoc::default()
            },
        )?;
        assert_eq!(named.name, "explicit");
        assert_eq!(named.definition.url, "http://example.test/mcp");
        Ok(())
    }

    #[test]
    fn log_metadata_does_not_change_the_configured_view_identity() -> Result<()> {
        let configuration = crate::server_config::parse_config(
            &PathBuf::from("synthetic.json"),
            r#"{"imports":[],"mcpServers":{"docs":{"command":"echo"}}}"#,
        )?;
        let selected = configuration
            .servers
            .get("docs")
            .context("configured server")?;
        let original = pool_name(selected, &selected.definition)?;
        let mut definition = selected.definition.clone();
        definition.configuration_entry = None;
        assert_eq!(original, pool_name(selected, &definition)?);
        definition.configuration_entry = Some(crate::config::ConfigurationEntry {
            source: PathBuf::from("metadata.json"),
            name: "metadata-only".to_owned(),
        });
        assert_eq!(original, pool_name(selected, &definition)?);
        Ok(())
    }
}
