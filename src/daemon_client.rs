use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::{self, ServerDef};
use crate::control::{ControlRequest, ControlResponse};
use crate::{diagnostics, transport};

pub(crate) async fn ensure_started(name: &str) -> anyhow::Result<()> {
    require_success(
        control_request(&ControlRequest::Start {
            name: name.to_string(),
        })
        .await?,
    )
}

pub(crate) async fn ensure_definition_started(
    name: &str,
    definition: &ServerDef,
) -> anyhow::Result<()> {
    require_success(
        control_request(&ControlRequest::StartDefinition {
            name: name.to_string(),
            definition: Box::new(definition.clone()),
        })
        .await?,
    )
}

fn require_success(response: ControlResponse) -> anyhow::Result<()> {
    if response.ok {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            response
                .error
                .unwrap_or_else(|| "pool startup failed".to_string())
        ))
    }
}

/// Retries once when the daemon closes the control connection without a response.
pub(crate) async fn control_request(request: &ControlRequest) -> anyhow::Result<ControlResponse> {
    let mut request_line = serde_json::to_string(request)?;
    request_line.push('\n');
    let response_line = match send_request(&request_line).await? {
        Some(line) => line,
        None => {
            diagnostics::log("control response empty; retrying once");
            send_request(&request_line)
                .await?
                .ok_or_else(|| anyhow::anyhow!("daemon closed control socket without responding"))?
        }
    };
    serde_json::from_str(response_line.trim())
        .map_err(|error| anyhow::anyhow!("parse control response: {error}"))
}

async fn send_request(request_line: &str) -> anyhow::Result<Option<String>> {
    let mut stream = ensure_daemon().await?;
    stream.write_all(request_line.as_bytes()).await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    let bytes = reader.read_line(&mut response_line).await?;
    Ok(if bytes == 0 {
        None
    } else {
        Some(response_line)
    })
}

async fn ensure_daemon() -> anyhow::Result<transport::LocalStream> {
    let socket_path = config::control_socket_path();
    match transport::connect(&socket_path).await {
        Ok(stream) => Ok(stream),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(error.into()),
        Err(_) => {
            spawn_daemon_detached()?;
            retry_connect(&socket_path).await
        }
    }
}

async fn retry_connect(socket_path: &std::path::Path) -> anyhow::Result<transport::LocalStream> {
    let mut last_error = None;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match transport::connect(socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(error.into());
            }
            Err(error) => last_error = Some(error),
        }
    }
    let error = last_error.unwrap_or_else(|| std::io::Error::other("control socket unreachable"));
    Err(anyhow::anyhow!(
        "could not reach daemon after launch ({error}). Try `mcp-pool pool serve` manually."
    ))
}

fn spawn_daemon_detached() -> anyhow::Result<()> {
    let mut arguments = vec!["pool".into(), "serve".into()];
    if diagnostics::is_enabled() {
        arguments.push("--debug".into());
    }
    crate::daemon_commands::launch::spawn_detached(&arguments)?;
    diagnostics::log("spawned daemon: pool serve");
    Ok(())
}
