use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::transport::LocalStream;

use super::McpRpcError;

const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

enum IncomingMessage {
    Notification(Value),
    Response(Value),
}

pub(super) async fn write_message(
    reader: &mut BufReader<LocalStream>,
    message: &Value,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    if bytes.len() >= MAX_FRAME_BYTES {
        bail!("MCP frame exceeds {MAX_FRAME_BYTES} bytes");
    }
    bytes.push(b'\n');
    reader.get_mut().write_all(&bytes).await?;
    reader.get_mut().flush().await?;
    Ok(())
}

// An outer error means the stream can no longer be trusted. An inner error is
// a valid JSON-RPC rejection and leaves the session available for another call.
pub(super) async fn receive_result(
    reader: &mut BufReader<LocalStream>,
    request_id: u64,
) -> Result<Result<Value>> {
    loop {
        let message = match receive_message(reader).await? {
            Some(IncomingMessage::Response(message)) => message,
            Some(IncomingMessage::Notification(_)) => continue,
            None => bail!("MCP connection reached EOF before a response"),
        };
        if message.get("id") != Some(&Value::from(request_id)) {
            bail!("MCP response ID does not match the outstanding request");
        }
        match (message.get("result"), message.get("error")) {
            (Some(result), None) => return Ok(Ok(result.clone())),
            (None, Some(error)) => {
                let code = error
                    .get("code")
                    .and_then(Value::as_i64)
                    .context("MCP JSON-RPC error omitted integer code")?;
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .context("MCP JSON-RPC error omitted message")?
                    .to_string();
                return Ok(Err(McpRpcError {
                    code,
                    message,
                    data: error.get("data").cloned(),
                }
                .into()));
            }
            _ => bail!("MCP response must contain exactly one of result or error"),
        }
    }
}

pub(super) async fn receive_notification(
    reader: &mut BufReader<LocalStream>,
) -> Result<Option<Value>> {
    match receive_message(reader).await? {
        Some(IncomingMessage::Notification(message)) => Ok(Some(message)),
        Some(IncomingMessage::Response(_)) => {
            bail!("unexpected MCP response on notification listener")
        }
        None => Ok(None),
    }
}

async fn receive_message(reader: &mut BufReader<LocalStream>) -> Result<Option<IncomingMessage>> {
    loop {
        let Some(message) = read_message(reader).await? else {
            return Ok(None);
        };
        if !message.is_object() || message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            bail!("invalid MCP JSON-RPC envelope");
        }
        if let Some(method) = message.get("method") {
            let method = method.as_str().context("MCP method must be a string")?;
            if message.get("result").is_some() || message.get("error").is_some() {
                bail!("MCP request/notification contains a response field");
            }
            if let Some(params) = message.get("params")
                && !params.is_object()
                && !params.is_array()
            {
                bail!("invalid MCP request/notification params");
            }
            if let Some(server_id) = message.get("id").filter(|identifier| !identifier.is_null()) {
                if !valid_id(server_id) {
                    bail!("invalid MCP server request ID");
                }
                let response = if method == "ping" {
                    json!({"jsonrpc":"2.0", "id":server_id, "result":{}})
                } else {
                    json!({
                        "jsonrpc":"2.0", "id":server_id,
                        "error":{"code":-32601, "message":format!("Unsupported client method: {method}")}
                    })
                };
                write_message(reader, &response).await?;
                continue;
            }
            return Ok(Some(IncomingMessage::Notification(message)));
        }
        return Ok(Some(IncomingMessage::Response(message)));
    }
}

fn valid_id(identifier: &Value) -> bool {
    identifier.is_string() || identifier.as_i64().is_some() || identifier.as_u64().is_some()
}

async fn read_message(reader: &mut BufReader<LocalStream>) -> Result<Option<Value>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf().await.context("reading MCP frame")?;
        if available.is_empty() {
            if frame.is_empty() {
                return Ok(None);
            }
            bail!("MCP connection reached EOF before a complete response frame");
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |position| position + 1);
        if frame.len().saturating_add(count) > MAX_FRAME_BYTES {
            bail!("MCP frame exceeds {MAX_FRAME_BYTES} bytes");
        }
        frame.extend_from_slice(
            available
                .get(..count)
                .context("invalid MCP read buffer range")?,
        );
        reader.consume(count);
        if newline.is_some() {
            if frame.iter().all(u8::is_ascii_whitespace) {
                frame.clear();
                continue;
            }
            return serde_json::from_slice(&frame)
                .map(Some)
                .context("invalid JSON in MCP frame");
        }
    }
}
