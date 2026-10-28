use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{artifacts, connect, persist_ad_hoc, print_outcome, server, stdin};
use crate::server_config::ServerConfiguration;
use crate::tool_arguments;

#[path = "tool_resolution.rs"]
mod resolution;
pub(super) use resolution::{has_real_tool, is_discovery_pseudo, resolve_tool};

pub(super) async fn run(configuration: &ServerConfiguration, arguments: Vec<String>) -> Result<()> {
    let has_explicit_json_input = arguments
        .iter()
        .any(|argument| matches!(argument.as_str(), "--args" | "--params" | "--json"));
    let mut parsed = tool_arguments::call(arguments)?;
    if parsed.inputs.has_stdin() {
        tool_arguments::merge_stdin(&mut parsed, &stdin().await?)?;
    }
    let selected = server(
        configuration,
        parsed.server.as_deref().unwrap_or("adhoc"),
        &parsed.ephemeral,
    )?;
    persist_ad_hoc(&selected, &parsed.ephemeral).await?;
    let mut progress = crate::cli_progress::Progress::new(!matches!(
        parsed.output,
        tool_arguments::Output::Json | tool_arguments::Output::Raw
    ));
    let mut tool = parsed.tool.clone();
    let mut discovery_fallback = None;
    let outcome: Result<Value> = async {
        let filter = super::tool_filter(&selected)?;
        let mut client = progress
            .wait(
                "Connecting to MCP server",
                connect(&selected, parsed.timeout, parsed.no_oauth),
            )
            .await?;
        let tools = progress
            .wait("Discovering MCP tools", client.list_tools())
            .await?;
        if let Some(requested) = tool.as_deref()
            && is_discovery_pseudo(requested)
            && !has_real_tool(&tools, requested)
        {
            if has_explicit_json_input
                || parsed.inputs.has_stdin()
                || !parsed.arguments.is_empty()
                || !parsed.positionals.is_empty()
                || !parsed.generic_flags.is_empty()
            {
                bail!("Pseudo-tool '{requested}' does not accept tool arguments or positionals");
            }
            discovery_fallback = Some((parsed.timeout, parsed.no_oauth, parsed.output));
            return Ok(Value::Null);
        }
        let resolved = super::resolve_tool(&selected, &tools, tool.as_deref(), &filter)?;
        tool = Some(resolved.clone());
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
        progress
            .wait(
                "Calling MCP tool",
                client.request(
                    "tools/call",
                    json!({"name":resolved,"arguments":parsed.arguments}),
                ),
            )
            .await
    }
    .await;
    progress.finish();
    if let Some((timeout, no_oauth, output)) = discovery_fallback {
        outcome?;
        crate::mcp_cli::discover_server(&selected, timeout, no_oauth, output).await?;
        return Ok(());
    }
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
