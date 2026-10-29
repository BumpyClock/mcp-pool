use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::server_config::{ConfiguredServer, ServerConfiguration};
use crate::tool_arguments::{AdHoc, Output, positive_milliseconds, value};

#[path = "tool_discovery_error.rs"]
mod errors;
#[cfg(test)]
use errors::category;
pub(crate) use errors::issue;

#[path = "tool_discovery_report.rs"]
mod report;
use report::{Outcome, ServerReport, View};

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

/// Reports servers in name order (the configuration map order) while discovering them concurrently.
pub async fn run(configuration: ServerConfiguration, arguments: Vec<String>) -> Result<()> {
    let mut flags = parse(arguments)?;
    let progress = crate::cli_progress::Progress::new(!flags.json && !flags.quiet);
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
    execute(selected, flags, selected_tool, detailed, progress).await
}

pub(crate) async fn single_server(
    server: ConfiguredServer,
    timeout: Option<u64>,
    no_oauth: bool,
    output: Output,
) -> Result<()> {
    let flags = Flags {
        json: matches!(output, Output::Json | Output::Raw),
        timeout,
        no_oauth,
        ..Flags::default()
    };
    let progress = crate::cli_progress::Progress::new(!flags.json && !flags.quiet);
    execute(vec![server], flags, None, true, progress).await
}

async fn execute(
    selected: Vec<ConfiguredServer>,
    flags: Flags,
    selected_tool: Option<String>,
    detailed: bool,
    mut progress: crate::cli_progress::Progress,
) -> Result<()> {
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
            let selected_tool = selected_tool.clone();
            tasks.spawn(async move {
                let report = discover(server, selected_tool.as_deref(), timeout, no_oauth).await;
                (index, report)
            });
        }
        let message = format!("Discovering MCP servers: {}/{total}", completed.len());
        match progress.wait(&message, tasks.join_next()).await {
            Some(result) => completed.push(result.context("Server discovery task failed")?),
            None => break,
        }
    }
    progress.finish();
    completed.sort_by_key(|(index, _)| *index);
    let reports: Vec<ServerReport> = completed.into_iter().map(|(_, report)| report).collect();
    let view = View {
        detailed,
        schema: flags.schema,
        brief: flags.brief,
        all_parameters: flags.all,
        sources: flags.sources,
    };
    if flags.json && !flags.quiet {
        let entries = reports
            .iter()
            .map(|report| report.to_json(&view))
            .collect::<Result<Vec<_>>>()?;
        let output = if detailed {
            entries
                .into_iter()
                .next()
                .context("Missing server discovery result")?
        } else {
            report::list_json(entries)
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else if !flags.quiet {
        let style = crate::tool_documentation::Style::terminal(flags.no_color);
        for report in &reports {
            report.print_text(&view, &style)?;
        }
        if total == 0 {
            println!("No MCP servers configured.");
        }
    }
    let tool_selection_failed = reports.iter().any(ServerReport::is_tool_selection_failure);
    let failed = reports.iter().any(ServerReport::is_failure);
    if tool_selection_failed || (failed && flags.exit_code) {
        bail!("MCP discovery failed");
    }
    Ok(())
}

async fn discover(
    server: ConfiguredServer,
    selected_tool: Option<&str>,
    timeout: Option<u64>,
    no_oauth: bool,
) -> ServerReport {
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
        let filter = server.tool_filter()?;
        let mut client = crate::mcp_cli::connect(&server, Some(timeout), no_oauth).await?;
        let tools = client.list_tools().await?;
        Ok::<_, anyhow::Error>((tools, filter, client.connection_info().cloned()))
    })
    .await;
    let duration = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let outcome = match outcome {
        Ok(Ok((tools, filter, connection))) => {
            prepare(&server, tools, &filter, connection, selected_tool)
        }
        Ok(Err(error)) => Outcome::DiscoveryFailed {
            error: crate::mcp_cli::safe_error(&server, &error),
        },
        Err(_) => Outcome::DiscoveryFailed {
            error: "Server discovery timed out".to_owned(),
        },
    };
    ServerReport {
        server,
        duration,
        outcome,
    }
}

/// Selection runs after timing so reported durations cover only server work.
fn prepare(
    server: &ConfiguredServer,
    mut tools: Vec<Value>,
    filter: &crate::tool_filter::ToolFilter,
    connection: Option<crate::mcp_protocol::ConnectionInfo>,
    selected_tool: Option<&str>,
) -> Outcome {
    tools.sort_by(|left, right| name(left).cmp(&name(right)));
    tools.dedup_by(|left, right| left.get("name") == right.get("name"));
    match selected_tool {
        Some(tool) => match crate::mcp_cli::resolve_tool(server, &tools, Some(tool), filter) {
            Ok(resolved) => tools.retain(|tool| name(tool) == Some(resolved.as_str())),
            Err(error) => {
                return Outcome::ToolSelectionFailed {
                    connection,
                    error: crate::mcp_cli::safe_error(server, &error),
                };
            }
        },
        None => tools.retain(|tool| name(tool).is_some_and(|name| filter.permits(name))),
    }
    Outcome::Listed { tools, connection }
}

fn name(tool: &Value) -> Option<&str> {
    tool.get("name").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn quiet_enables_health_exit_code_and_conflicts_are_explicit() -> Result<()> {
        let parsed = parse(vec!["docs".into(), "--quiet".into()])?;
        assert!(parsed.quiet && parsed.exit_code);
        assert!(parse(vec!["--brief".into(), "--json".into()]).is_err());
        assert!(parse(vec!["--timeout".into(), "0".into()]).is_err());
        Ok(())
    }

    #[tokio::test]
    async fn invalid_tool_filters_fail_before_connecting() -> Result<()> {
        let server = ConfiguredServer {
            name: "fixture".to_owned(),
            definition: crate::config::ServerDef::default(),
            raw: json!({"allowedTools":[],"blockedTools":[]}),
            source: std::path::PathBuf::from("fixture.json"),
        };
        let report = discover(server, None, None, false).await;
        assert!(matches!(
            report.outcome,
            Outcome::DiscoveryFailed { error } if error.contains("invalid tool filters")
        ));
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
