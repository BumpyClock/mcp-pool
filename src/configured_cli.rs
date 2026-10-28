use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::json;

use crate::control::{ControlRequest, ControlResponse};
use crate::daemon_client::ownership::{self, Selection};
use crate::server_config::ConfiguredServer;

pub(crate) enum Operation {
    Start { name: String },
    StartAll,
    Stop { name: String },
    Restart { name: String },
    Status { name: Option<String> },
    Shutdown,
}

pub(crate) async fn start(server: &ConfiguredServer) -> Result<ControlResponse> {
    let definition = crate::oauth::prepare(server, false).await?;
    let name = crate::mcp_cli::pool_name(server, &definition)?;
    crate::daemon_client::control_request(&ControlRequest::StartDefinition {
        name,
        definition: Box::new(definition),
    })
    .await
}

pub(crate) async fn control(
    request: &Operation,
    path: Option<PathBuf>,
    root: Option<&Path>,
) -> Result<ControlResponse> {
    if matches!(
        request,
        Operation::Shutdown | Operation::Status { name: None }
    ) {
        let request = if matches!(request, Operation::Shutdown) {
            ControlRequest::Shutdown
        } else {
            ControlRequest::Status { name: None }
        };
        return ownership::readable(crate::daemon_client::control_request(&request).await?, None);
    }
    let configuration = crate::server_config::load_at(path, root)?;
    if matches!(request, Operation::StartAll) {
        let mut tasks = tokio::task::JoinSet::new();
        for (name, server) in configuration.servers {
            tasks.spawn(async move {
                let result = start(&server).await;
                match result {
                    Ok(response) if response.ok => json!({"name":name,"ok":true}),
                    Ok(response) => json!({"name":name,"ok":false,"error":response.error}),
                    Err(error) => json!({"name":name,"ok":false,"error":error.to_string()}),
                }
            });
        }
        let mut servers = Vec::new();
        while let Some(result) = tasks.join_next().await {
            servers.push(result.context("Starting configured server task")?);
        }
        return Ok(ControlResponse::data(json!({"servers":servers})));
    }
    let name = match request {
        Operation::Start { name }
        | Operation::Stop { name }
        | Operation::Restart { name }
        | Operation::Status { name: Some(name) } => name,
        _ => anyhow::bail!("Unexpected configured control operation"),
    };
    let server = configuration
        .servers
        .get(name)
        .with_context(|| format!("Unknown MCP server '{name}'"))?;
    if matches!(request, Operation::Start { .. }) {
        return start(server).await;
    }
    let response =
        crate::daemon_client::control_request(&ControlRequest::Status { name: None }).await?;
    if !response.ok {
        return Ok(response);
    }
    let selection = Selection::Native {
        source: &server.source,
        name: &server.name,
        scope: server.definition.credential_scope.as_deref(),
    };
    if matches!(request, Operation::Status { .. }) {
        return ownership::readable(response, Some(selection));
    }
    let status = ownership::status(&response)?;
    let servers = status
        .servers
        .iter()
        .map(|server| selection.matches(server).map(|selected| (server, selected)))
        .collect::<Result<Vec<_>>>()?;
    for (server, selected) in servers {
        if !selected {
            continue;
        }
        let name = server.name.clone();
        let operation = if matches!(request, Operation::Restart { .. }) {
            ControlRequest::Restart { name }
        } else {
            ControlRequest::Stop { name }
        };
        let response = crate::daemon_client::control_request(&operation).await?;
        if !response.ok {
            return Ok(response);
        }
    }
    Ok(ControlResponse::ok())
}
