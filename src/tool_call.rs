use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{artifacts, connect, persist_ad_hoc, print_outcome, server, stdin, tool_allowed};
use crate::server_config::{ConfiguredServer, ServerConfiguration};
use crate::tool_arguments;

pub(super) async fn run(configuration: &ServerConfiguration, arguments: Vec<String>) -> Result<()> {
    let mut parsed = tool_arguments::call(arguments)?;
    if parsed.stdin {
        tool_arguments::merge_stdin(&mut parsed, &stdin().await?)?;
    }
    let selected = server(
        configuration,
        parsed.server.as_deref().unwrap_or("adhoc"),
        &parsed.ephemeral,
    )?;
    persist_ad_hoc(&selected, &parsed.ephemeral).await?;
    let mut tool = parsed.tool.clone();
    let outcome: Result<Value> = async {
        let mut client = connect(&selected, parsed.timeout, parsed.no_oauth).await?;
        let tools = client.list_tools().await?;
        let resolved = select_tool(&selected, &tools, tool.as_deref())?;
        tool = Some(resolved.clone());
        if !tool_allowed(&selected, &resolved)? {
            bail!(
                "Tool '{resolved}' is excluded by the configuration for '{}'",
                selected.name
            );
        }
        let metadata = tools
            .iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(&resolved))
            .with_context(|| format!("Tool '{resolved}' not found on '{}'", selected.name))?;
        if let Some(schema) = metadata.get("inputSchema") {
            tool_arguments::hydrate(&mut parsed, schema)?;
        } else if !parsed.positionals.is_empty() {
            bail!("Tool has no schema for positional arguments; use key=value");
        } else if !parsed.generic_flags.is_empty() {
            bail!("Tool declares no schema options; use key=value or --args for named arguments");
        }
        client
            .request(
                "tools/call",
                json!({"name":resolved,"arguments":parsed.arguments}),
            )
            .await
    }
    .await;
    let log_result = outcome.as_ref().ok().cloned();
    let rendered = print_outcome(&selected, tool.as_deref(), outcome, parsed.output);
    if let Some(result) = &log_result
        && let Err(error) = artifacts::save_images(result, parsed.save_images.as_deref()).await
    {
        eprintln!("[mcp-pool] Tool result completed; image export failed: {error:#}");
        rendered?;
        return Err(error).context("Image export failed after the completed tool result was rendered; do not retry the tool call");
    }
    if parsed.tail_log
        && let Some(result) = log_result
    {
        artifacts::tail_log(&result).await?;
    }
    rendered
}

pub(super) fn select_tool(
    server: &ConfiguredServer,
    tools: &[Value],
    requested: Option<&str>,
) -> Result<String> {
    if let Some(name) = requested {
        return Ok(name.to_owned());
    }
    let mut eligible = Vec::new();
    for name in tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
    {
        if tool_allowed(server, name)? {
            eligible.push(name);
        }
    }
    match eligible.as_slice() {
        [name] => Ok((*name).to_owned()),
        [] => bail!("Server '{}' exposes no available tools", server.name),
        _ => bail!(
            "Missing tool: '{}' exposes multiple tools; use call {}.TOOL",
            server.name,
            server.name
        ),
    }
}
