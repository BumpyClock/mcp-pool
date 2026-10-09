use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::time::{Instant, timeout, timeout_at};

use crate::transport::LocalStream;

#[path = "mcp_client_wire.rs"]
mod wire;

const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// A sequential MCP session over one shared pool connection.
/// Cancellation closes the local connection because the pool does not translate
/// request IDs, so cancellation could target another client's upstream request.
pub struct McpClient {
    reader: Option<BufReader<LocalStream>>,
    next_id: u64,
    deadline: Duration,
}

/// A server JSON-RPC error, distinct from transport and protocol failures.
#[derive(Debug, thiserror::Error)]
#[error("JSON-RPC error {code}: {message}")]
pub struct McpRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

impl McpClient {
    pub fn is_closed(&self) -> bool {
        self.reader.is_none()
    }

    /// The deadline covers pool startup, socket connection, and initialization.
    pub async fn connect(
        name: &str,
        definition: &crate::config::ServerDef,
        deadline: Duration,
    ) -> Result<Self> {
        let expires = operation_expiry(deadline)?;
        timeout_at(expires, async {
            crate::daemon_client::ensure_definition_started(name, definition).await?;
            let socket_path = crate::config::server_socket_path(name);
            let stream = loop {
                match crate::transport::connect(&socket_path).await {
                    Ok(stream) => break stream,
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::ConnectionRefused
                                | std::io::ErrorKind::WouldBlock
                        ) || (cfg!(windows) && error.raw_os_error() == Some(231)) =>
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(error) => return Err(error).context("connecting to MCP pool socket"),
                }
            };
            Self::initialize(stream, deadline).await
        })
        .await
        .context("MCP connection/initialization deadline exceeded")?
    }

    pub(crate) async fn initialize(stream: LocalStream, deadline: Duration) -> Result<Self> {
        operation_expiry(deadline)?;
        let mut client = Self {
            reader: Some(BufReader::new(stream)),
            next_id: 1,
            deadline,
        };
        let result = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "mcp-pool", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .context("MCP initialize result omitted protocolVersion")?;
        if !SUPPORTED_PROTOCOL_VERSIONS.contains(&version) {
            bail!("unsupported MCP protocol version: {version}");
        }
        if !result.get("capabilities").is_some_and(Value::is_object) {
            bail!("MCP initialize result omitted capabilities");
        }
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    /// Returns the matched JSON-RPC result; server errors remain downcastable to
    /// `McpRpcError`. Timeout, EOF, or malformed frames retire this connection.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        validate_parameters(method, &params)?;
        let timeout_ms = timeout_milliseconds(self.deadline)?;
        let request_id = self.next_id;
        self.next_id = request_id
            .checked_add(1)
            .context("MCP request IDs exhausted")?;
        let mut reader = self.reader.take().context("MCP connection is closed")?;
        let outcome = timeout(self.deadline, async {
            let message = json!({
                "jsonrpc":"2.0", "id":request_id, "method":method, "params":params,
                crate::request_deadline::TIMEOUT_FIELD:timeout_ms
            });
            wire::write_message(&mut reader, &message).await?;
            wire::receive_result(&mut reader, request_id).await
        })
        .await
        .with_context(|| format!("MCP {method} deadline exceeded; request outcome is unknown"))??;
        self.reader = Some(reader);
        outcome
    }

    pub async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        validate_parameters(method, &params)?;
        let mut reader = self.reader.take().context("MCP connection is closed")?;
        timeout(
            self.deadline,
            wire::write_message(
                &mut reader,
                &json!({"jsonrpc":"2.0", "method":method, "params":params}),
            ),
        )
        .await
        .with_context(|| format!("MCP {method} notification deadline exceeded"))??;
        self.reader = Some(reader);
        Ok(())
    }

    /// Dedicated notification connections have no idle deadline; requests
    /// consume intervening notifications and need a separate connection.
    /// Cancellation retires this local socket.
    pub async fn wait_for_notification(&mut self) -> Result<Option<Value>> {
        let mut reader = self.reader.take().context("MCP connection is closed")?;
        let notification = wire::receive_notification(&mut reader).await?;
        if notification.is_some() {
            self.reader = Some(reader);
        }
        Ok(notification)
    }

    /// Collects every page while leaving the cacheable first page cursor-free.
    pub async fn list_tools(&mut self) -> Result<Vec<Value>> {
        self.list_entries("tools/list", "tools").await
    }

    pub async fn list_resources(&mut self) -> Result<Vec<Value>> {
        self.list_entries("resources/list", "resources").await
    }

    async fn list_entries(&mut self, method: &str, field: &str) -> Result<Vec<Value>> {
        match timeout(self.deadline, self.collect_pages(method, field)).await {
            Ok(result) => result,
            Err(error) => {
                self.reader = None;
                Err(error).with_context(|| format!("MCP {method} pagination deadline exceeded"))
            }
        }
    }

    async fn collect_pages(&mut self, method: &str, field: &str) -> Result<Vec<Value>> {
        let mut entries = Vec::new();
        let mut params = json!({});
        let mut cursors = HashSet::new();
        loop {
            let result = self.request(method, params).await?;
            let page = result
                .get(field)
                .and_then(Value::as_array)
                .with_context(|| format!("MCP {method} result omitted {field} array"))?;
            if page.iter().any(|entry| !entry.is_object()) {
                bail!("MCP {method} returned a non-object {field} entry");
            }
            entries.extend(page.iter().cloned());
            match result.get("nextCursor") {
                None => return Ok(entries),
                Some(Value::String(cursor)) => {
                    if !cursors.insert(cursor.clone()) {
                        bail!("MCP {method} repeated pagination cursor");
                    }
                    params = json!({"cursor":cursor});
                }
                Some(_) => bail!("MCP {method} nextCursor must be a string"),
            }
        }
    }
}

fn operation_expiry(deadline: Duration) -> Result<Instant> {
    if deadline.is_zero() {
        bail!("MCP operation deadline must be positive");
    }
    Instant::now()
        .checked_add(deadline)
        .context("MCP operation deadline is too large")
}

/// Rounds up so a positive local deadline never becomes a zero backend timeout.
fn timeout_milliseconds(deadline: Duration) -> Result<u64> {
    if deadline.is_zero() {
        bail!("MCP operation deadline must be positive");
    }
    u64::try_from(deadline.as_nanos().div_ceil(1_000_000))
        .context("MCP operation deadline exceeds supported milliseconds")
}

fn validate_parameters(method: &str, params: &Value) -> Result<()> {
    if method == "notifications/cancelled" {
        bail!("MCP cancellation is unsafe: the pool does not translate requestId");
    }
    if method.is_empty() {
        bail!("MCP method must not be empty");
    }
    if !params.is_object() && !params.is_array() {
        bail!("JSON-RPC params must be an object or array");
    }
    Ok(())
}

#[cfg(test)]
#[path = "mcp_client_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mcp_client_deadline_tests.rs"]
mod deadline_tests;

#[cfg(test)]
#[path = "mcp_client_notification_tests.rs"]
mod notification_tests;
