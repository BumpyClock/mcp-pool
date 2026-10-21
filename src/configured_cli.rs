use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::control::{ControlRequest, ControlResponse};
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
        return readable(crate::daemon_client::control_request(&request).await?);
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
    let mut response =
        crate::daemon_client::control_request(&ControlRequest::Status { name: None }).await?;
    if !response.ok {
        return Ok(response);
    }
    let servers = response
        .data
        .as_mut()
        .and_then(|value| value.get_mut("servers"))
        .and_then(Value::as_array_mut)
        .context("Daemon status omitted servers")?;
    if servers.iter().any(|value| {
        value.pointer("/configuration_entry/source") == Some(&json!(server.source))
            && value
                .pointer("/configuration_entry/name")
                .and_then(Value::as_str)
                == Some(server.name.as_str())
            && value.pointer("/configuration_entry/scope").is_none()
    }) {
        anyhow::bail!(
            "Daemon lacks credential-scope ownership metadata; restart the daemon after upgrading"
        );
    }
    servers.retain(|value| matches_entry(value, server));
    if matches!(request, Operation::Status { .. }) {
        return readable(response);
    }
    for value in servers {
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .context("Daemon status omitted name")?
            .to_owned();
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

fn matches_entry(value: &Value, server: &ConfiguredServer) -> bool {
    value.get("configuration_entry").is_some_and(|entry| {
        entry.get("source") == Some(&json!(server.source))
            && entry.get("name").and_then(Value::as_str) == Some(server.name.as_str())
            && entry.get("scope")
                == server
                    .definition
                    .credential_scope
                    .as_ref()
                    .map(|scope| json!(scope))
                    .as_ref()
    })
}

fn readable(mut response: ControlResponse) -> Result<ControlResponse> {
    if let Some(servers) = response
        .data
        .as_mut()
        .and_then(|data| data.get_mut("servers"))
        .and_then(Value::as_array_mut)
    {
        for server in servers {
            if let Some(name) = server
                .pointer("/configuration_entry/name")
                .and_then(Value::as_str)
                .map(str::to_owned)
            {
                let object = server
                    .as_object_mut()
                    .context("Invalid daemon status server")?;
                if let Some(pool_name) = object.insert("name".into(), json!(name)) {
                    object.insert("pool_name".into(), pool_name);
                }
            }
        }
    }
    Ok(response)
}
