use anyhow::{Context, Result, bail};
use serde_json::json;

use super::{connect, print_outcome, server};
use crate::server_config::ServerConfiguration;
use crate::tool_arguments::{self, AdHoc, Output};

pub(super) async fn run(configuration: &ServerConfiguration, arguments: Vec<String>) -> Result<()> {
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
    let mut progress =
        crate::cli_progress::Progress::new(!matches!(output, Output::Json | Output::Raw));
    let outcome = async {
        let mut client = progress
            .wait(
                "Connecting to MCP server",
                connect(&selected, timeout, no_oauth),
            )
            .await?;
        if let Some(uri) = positional.get(1) {
            progress
                .wait(
                    "Reading MCP resource",
                    client.request("resources/read", json!({"uri":uri})),
                )
                .await
        } else {
            progress
                .wait("Discovering MCP resources", client.list_resources())
                .await
                .map(|resources| json!({"resources":resources}))
        }
    }
    .await;
    progress.finish();
    print_outcome(&selected, None, outcome, output)
}
