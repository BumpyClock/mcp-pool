use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::server_config::{ConfiguredServer, ServerConfiguration};
use crate::tool_arguments::{AdHoc, Output, positive_milliseconds, value};

#[path = "tool_discovery_error.rs"]
mod errors;
use errors::attach_error;
#[cfg(test)]
use errors::category;
pub(crate) use errors::issue;

#[derive(Debug, thiserror::Error)]
#[error("MCP discovery failed")]
pub(crate) struct QuietFailure;

#[derive(Default)]
struct Flags {
    json: bool,
    schema: bool,
    brief: bool,
    all: bool,
    quiet: bool,
    no_color: bool,
    exit_code: bool,
    status: bool,
    timeout: Option<u64>,
    no_oauth: bool,
    sources: bool,
    ephemeral: AdHoc,
    target: Option<String>,
}

struct Discovery {
    server: ConfiguredServer,
    tools: Vec<Value>,
    duration: u64,
    error: Option<String>,
    connection_info: Option<crate::mcp_protocol::ConnectionInfo>,
}

fn parse(arguments: Vec<String>) -> Result<Flags> {
    let mut flags = Flags::default();
    let mut arguments = VecDeque::from(arguments);
    while let Some(argument) = arguments.pop_front() {
        if flags.ephemeral.consume(&argument, &mut arguments)? {
            continue;
        }
        match argument.as_str() {
            "--json" => flags.json = true,
            "--no-color" => flags.no_color = true,
            "--schema" => flags.schema = true,
            "--brief" | "--signatures" => flags.brief = true,
            "--all-parameters" => flags.all = true,
            "--quiet" => {
                flags.quiet = true;
                flags.exit_code = true;
            }
            "--exit-code" => flags.exit_code = true,
            "--status" => flags.status = true,
            "--no-oauth" => flags.no_oauth = true,
            "--sources" | "--verbose" => flags.sources = true,
            "--yes" => {}
            "--timeout" => {
                flags.timeout = Some(positive_milliseconds(&value(&mut arguments, &argument)?)?)
            }
            "--output" => match Output::parse(&value(&mut arguments, &argument)?)? {
                Output::Json => flags.json = true,
                Output::Text => flags.json = false,
                _ => bail!("list --output supports text or json only"),
            },
            _ if argument.starts_with('-') => bail!("Unknown list flag '{argument}'"),
            _ => {
                if flags.target.is_some() {
                    bail!("Usage: list [SERVER[.TOOL]] [flags]");
                }
                flags.target = Some(argument);
            }
        }
    }
    if flags.brief && (flags.schema || flags.json || flags.status || flags.all || flags.sources) {
        bail!(
            "--brief/--signatures cannot be combined with --schema, --json, --status, --all-parameters, or --verbose/--sources"
        );
    }
    if flags.status && (flags.schema || flags.all) {
        bail!("--status cannot be combined with --schema or --all-parameters");
    }
    Ok(flags)
}

/// Preserves configured server order in output while discovering servers concurrently.
pub async fn run(configuration: ServerConfiguration, arguments: Vec<String>) -> Result<()> {
    let mut flags = parse(arguments)?;
    let mut progress = crate::cli_progress::Progress::new(!flags.json && !flags.quiet);
    let mut selected_url_tool = None;
    if let Some(target) = &flags.target
        && let Some((url, tool)) = crate::mcp_cli::selector::split_http(target)?
    {
        flags.ephemeral.url = Some(url);
        selected_url_tool = tool;
    }
    flags.ephemeral.validate()?;
    let mut selected_tool = selected_url_tool;
    let selected: Vec<ConfiguredServer> = if flags.ephemeral.present() {
        vec![crate::mcp_cli::server(
            &configuration,
            flags.target.as_deref().unwrap_or("adhoc"),
            &flags.ephemeral,
        )?]
    } else if let Some(target) = &flags.target {
        if configuration.servers.contains_key(target)
            || target.starts_with("https://")
            || target.starts_with("http://")
        {
            vec![crate::mcp_cli::server(
                &configuration,
                target,
                &flags.ephemeral,
            )?]
        } else if let Some((server, tool)) = target.split_once('.') {
            selected_tool = Some(tool.to_owned());
            vec![crate::mcp_cli::server(
                &configuration,
                server,
                &flags.ephemeral,
            )?]
        } else {
            bail!("Unknown MCP server '{target}'");
        }
    } else {
        configuration.servers.values().cloned().collect()
    };
    if flags.ephemeral.persist.is_some() {
        let server = selected
            .first()
            .context("--persist requires one ad-hoc server")?;
        crate::mcp_cli::persist_ad_hoc(server, &flags.ephemeral).await?;
    }
    let detailed = (flags.target.is_some() || flags.ephemeral.present()) && !flags.status;
    let mut entries = Vec::new();
    let mut failure = false;
    let mut tool_selection_failed = false;
    let total = selected.len();
    let mut pending = VecDeque::from(selected);
    let mut tasks = tokio::task::JoinSet::new();
    let mut completed = Vec::new();
    let mut next_index = 0usize;
    loop {
        while tasks.len() < 4 {
            let Some(server) = pending.pop_front() else {
                break;
            };
            let index = next_index;
            next_index += 1;
            let timeout = flags.timeout;
            let no_oauth = flags.no_oauth;
            tasks.spawn(async move { (index, discover(server, timeout, no_oauth).await) });
        }
        let message = format!("Discovering MCP servers: {}/{total}", completed.len());
        match progress.wait(&message, tasks.join_next()).await {
            Some(result) => completed.push(result.context("Server discovery task failed")?),
            None => break,
        }
    }
    progress.finish();
    let style = crate::tool_documentation::Style::terminal(flags.no_color);
    completed.sort_by_key(|(index, _)| *index);
    for (_, discovered) in completed {
        let Discovery {
            server,
            mut tools,
            duration,
            error,
            connection_info,
        } = discovered;
        let mut entry = base(&server, duration);
        if let Some(info) = &connection_info
            && let Some(object) = entry.as_object_mut()
            && let Value::Object(metadata) = serde_json::to_value(info)?
        {
            object.extend(metadata);
        }
        if let Some(error) = error {
            failure = true;
            attach_error(&mut entry, &server.name, &error);
        } else {
            tools.sort_by(|left, right| {
                left.get("name")
                    .and_then(Value::as_str)
                    .cmp(&right.get("name").and_then(Value::as_str))
            });
            tools.dedup_by(|left, right| left.get("name") == right.get("name"));
            if let Some(tool) = &selected_tool {
                match crate::mcp_cli::resolve_tool(&server, &tools, tool) {
                    Ok(resolved) => tools.retain(|entry| {
                        entry.get("name").and_then(Value::as_str) == Some(resolved.as_str())
                    }),
                    Err(error) => {
                        failure = true;
                        tool_selection_failed = true;
                        tools.clear();
                        attach_error(
                            &mut entry,
                            &server.name,
                            &crate::mcp_cli::safe_error(&server, &error),
                        );
                    }
                }
            }
            let rendered_tools: Vec<Value> = tools
                .iter()
                .map(|tool| {
                    let mut metadata = serde_json::Map::new();
                    for key in ["name", "description"] {
                        if let Some(value) = tool.get(key) {
                            metadata.insert(key.to_owned(), value.clone());
                        }
                    }
                    if detailed || flags.schema {
                        for key in ["inputSchema", "outputSchema"] {
                            if let Some(value) = tool.get(key) {
                                metadata.insert(key.to_owned(), value.clone());
                            }
                        }
                    }
                    if detailed {
                        metadata.insert(
                            "options".to_owned(),
                            json!(crate::tool_output::options(tool)),
                        );
                    }
                    Value::Object(metadata)
                })
                .collect();
            if let Some(object) = entry.as_object_mut() {
                object.insert("tools".to_owned(), json!(rendered_tools));
            }
            if !flags.json && !flags.quiet {
                if detailed {
                    if server.definition.description.is_empty() {
                        println!("{}", style.heading(&server.name));
                    } else {
                        println!(
                            "{} - {}",
                            style.heading(&server.name),
                            style.muted(&server.definition.description)
                        );
                    }
                    println!();
                    if let Some(instructions) = connection_info
                        .as_ref()
                        .and_then(|info| info.instructions.as_deref())
                        .filter(|instructions| !instructions.is_empty())
                    {
                        println!("{}\n", style.muted(instructions));
                    }
                    let mut optional_hidden = false;
                    for tool in &tools {
                        if flags.brief {
                            println!("  {}", style.brief(tool));
                        } else {
                            let documentation = style.render(tool, flags.all);
                            optional_hidden |= documentation.hidden_parameters;
                            println!("{}", documentation.text);
                        }
                        if flags.schema
                            && let Some(schema) = tool.get("inputSchema")
                        {
                            println!("{}", serde_json::to_string_pretty(schema)?);
                        }
                    }
                    if !flags.brief
                        && let Some(tool) = tools.first()
                    {
                        println!("  {}", style.heading("Examples:"));
                        println!("    {}\n", style.example(&server.name, tool));
                    }
                    if optional_hidden {
                        println!("{}\n", style.muted("  Optional parameters hidden; run with --all-parameters to view all fields."));
                    }
                    println!(
                        "{}",
                        style.muted(&format!(
                            "  {} tools · {duration}ms · {}",
                            tools.len(),
                            display_transport(&server)
                        ))
                    );
                } else {
                    println!("{} ({} tools; {duration}ms)", server.name, tools.len());
                }
            }
        }
        if flags.sources
            && let Some(object) = entry.as_object_mut()
            && let Some(source) = object.get("source").cloned()
        {
            object.insert("sources".to_owned(), json!([source]));
        }
        if entry.get("status").and_then(Value::as_str) != Some("ok") && !flags.json && !flags.quiet
        {
            println!(
                "{} — {}",
                server.name,
                entry
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("Discovery failed")
            );
        }
        if detailed && let Some(object) = entry.as_object_mut() {
            object.insert("mode".to_owned(), json!("server"));
        }
        entries.push(entry);
    }
    if flags.json && !flags.quiet {
        let output = if detailed {
            entries
                .into_iter()
                .next()
                .context("Missing server discovery result")?
        } else {
            let mut counts = json!({"ok":0,"auth":0,"offline":0,"http":0,"error":0});
            for entry in &entries {
                let status = entry
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("error");
                if let Some(count) = counts.get_mut(status) {
                    *count = json!(count.as_u64().unwrap_or(0) + 1);
                }
            }
            json!({"mode":"list","counts":counts,"servers":entries})
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if next_index == 0 && !flags.quiet {
        println!("No MCP servers configured.");
    }
    if failure && (tool_selection_failed || flags.exit_code) {
        bail!("MCP discovery failed");
    }
    Ok(())
}

async fn discover(server: ConfiguredServer, timeout: Option<u64>, no_oauth: bool) -> Discovery {
    let started = Instant::now();
    let environment = std::env::var("MCPORTER_LIST_TIMEOUT").ok();
    let timeout = crate::mcp_cli::context::timeout(
        timeout,
        environment.as_deref(),
        server.definition.timeout_ms,
        30_000,
    );
    let duration = Duration::from_millis(timeout);
    let outcome = tokio::time::timeout(duration, async {
        let mut client = crate::mcp_cli::connect(&server, Some(timeout), no_oauth).await?;
        let tools = client.list_tools().await?;
        crate::mcp_cli::tool_allowed(&server, "")?;
        Ok::<_, anyhow::Error>((
            tools
                .into_iter()
                .filter(|tool| {
                    tool.get("name")
                        .and_then(Value::as_str)
                        .is_some_and(|name| {
                            crate::mcp_cli::tool_allowed(&server, name).unwrap_or(false)
                        })
                })
                .collect(),
            client.connection_info().cloned(),
        ))
    })
    .await;
    let duration = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    match outcome {
        Ok(Ok((tools, connection_info))) => Discovery {
            server,
            tools,
            duration,
            error: None,
            connection_info,
        },
        Ok(Err(error)) => {
            let message = crate::mcp_cli::safe_error(&server, &error);
            Discovery {
                server,
                tools: Vec::new(),
                duration,
                error: Some(message),
                connection_info: None,
            }
        }
        Err(_) => Discovery {
            server,
            tools: Vec::new(),
            duration,
            error: Some("Server discovery timed out".to_owned()),
            connection_info: None,
        },
    }
}

fn base(server: &ConfiguredServer, duration: u64) -> Value {
    let mut value = json!({
        "name":server.name,"status":"ok","durationMs":duration,
        "transport":transport(server),"source":{"kind":"local","path":server.source},
    });
    if !server.definition.description.is_empty()
        && let Some(object) = value.as_object_mut()
    {
        object.insert(
            "description".to_owned(),
            json!(server.definition.description),
        );
    }
    value
}

fn transport(server: &ConfiguredServer) -> String {
    if server.definition.is_remote() {
        let sanitized = crate::mcp_cli::display_url(&server.definition.url);
        format!("HTTP {sanitized}")
    } else {
        format!("STDIO {}", server.definition.command)
    }
}

fn display_transport(server: &ConfiguredServer) -> String {
    if server.definition.transport_kind() == "sse" {
        format!(
            "SSE {}",
            crate::mcp_cli::display_url(&server.definition.url)
        )
    } else {
        transport(server)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_transport_preserves_sse_without_changing_json_metadata() {
        let mut server = ConfiguredServer {
            name: "fixture".to_owned(),
            definition: crate::config::ServerDef {
                url: "https://username:password@example.com/mcp?token=secret#fragment".to_owned(),
                transport: "SSE".to_owned(),
                ..Default::default()
            },
            raw: Value::Null,
            source: std::path::PathBuf::from("fixture.json"),
        };
        assert_eq!(display_transport(&server), "SSE https://example.com/mcp");
        assert_eq!(
            base(&server, 0).get("transport"),
            Some(&json!("HTTP https://example.com/mcp"))
        );
        server.definition.transport = "http".to_owned();
        assert_eq!(display_transport(&server), "HTTP https://example.com/mcp");
        server.definition.url.clear();
        server.definition.command = "fixture-command".to_owned();
        assert_eq!(display_transport(&server), "STDIO fixture-command");
    }

    #[test]
    fn quiet_enables_health_exit_code_and_conflicts_are_explicit() -> Result<()> {
        let parsed = parse(vec!["docs".into(), "--quiet".into()])?;
        assert!(parsed.quiet && parsed.exit_code);
        assert!(parse(vec!["--brief".into(), "--json".into()]).is_err());
        assert!(parse(vec!["--timeout".into(), "0".into()]).is_err());
        Ok(())
    }

    #[test]
    fn authentication_classification_does_not_confuse_ports_and_timeouts() {
        assert_eq!(
            category("connect timeout after 4010ms to 127.0.0.1:14012"),
            "offline"
        );
        assert_eq!(category("JSON-RPC error -32603: backend failed"), "error");
        assert_eq!(category("HTTP status 403 Forbidden"), "auth");
        assert_eq!(
            issue("HTTP status 503").get("statusCode"),
            Some(&json!(503))
        );
    }
}
