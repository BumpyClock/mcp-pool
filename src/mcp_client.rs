use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::BufReader;
use tokio::time::{Instant, timeout, timeout_at};

use crate::mcp_protocol::{self, ConnectionInfo, Era, ProtocolPreference};
use crate::transport::LocalStream;

#[path = "mcp_client_wire.rs"]
mod wire;

/// A sequential MCP session over one shared pool connection.
/// Cancellation closes the local connection because the pool does not translate
/// request IDs, so cancellation could target another client's upstream request.
pub struct McpClient {
    reader: Option<BufReader<LocalStream>>,
    next_id: u64,
    deadline: Duration,
    connection_info: Option<ConnectionInfo>,
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
        let preference = match (definition.protocol_version, definition.transport_kind()) {
            (ProtocolPreference::Modern, "sse") => {
                bail!("Pinned modern MCP requires stdio or HTTP, not legacy SSE");
            }
            (ProtocolPreference::Auto, "sse") => ProtocolPreference::Legacy,
            (preference, _) => preference,
        };
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
            Self::negotiate(stream, deadline, preference).await
        })
        .await
        .context("MCP connection/initialization deadline exceeded")?
    }

    #[cfg(test)]
    pub(crate) async fn initialize(stream: LocalStream, deadline: Duration) -> Result<Self> {
        Self::negotiate(stream, deadline, ProtocolPreference::Legacy).await
    }

    pub(crate) fn connection_info(&self) -> Option<&ConnectionInfo> {
        self.connection_info.as_ref()
    }

    async fn negotiate(
        stream: LocalStream,
        deadline: Duration,
        preference: ProtocolPreference,
    ) -> Result<Self> {
        operation_expiry(deadline)?;
        let mut client = Self {
            reader: Some(BufReader::new(stream)),
            next_id: 1,
            deadline,
            connection_info: None,
        };
        if preference != ProtocolPreference::Legacy {
            let mut parameters = json!({});
            mcp_protocol::stamp(&mut parameters)?;
            match client.request("server/discover", parameters).await {
                Ok(result) if mcp_protocol::discovery_offers_modern(&result)? => {
                    client.connection_info =
                        Some(mcp_protocol::connection_info(&result, Era::Modern)?);
                    return Ok(client);
                }
                Ok(_) if preference == ProtocolPreference::Auto => {}
                Err(error)
                    if preference == ProtocolPreference::Auto
                        && Self::discovery_unsupported(&error) => {}
                Ok(_) => {
                    bail!(
                        "MCP server does not support pinned protocol version {}",
                        mcp_protocol::MODERN_VERSION
                    );
                }
                Err(error) => return Err(error).context(
                    "MCP protocol discovery failed; no upstream or operation was replayed. For a legacy server that cannot handle discovery, set protocolVersion to legacy",
                ),
            }
        }
        let result = client
            .request(
                "initialize",
                json!({
                    "protocolVersion": mcp_protocol::LEGACY_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "mcp-pool", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        client.connection_info = Some(mcp_protocol::connection_info(&result, Era::Legacy)?);
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        Ok(client)
    }

    /// Returns the matched JSON-RPC result; server errors remain downcastable to
    /// `McpRpcError`. Timeout, EOF, or malformed frames retire this connection.
    pub async fn request(&mut self, method: &str, mut params: Value) -> Result<Value> {
        if self
            .connection_info
            .as_ref()
            .is_some_and(|info| info.era == Era::Modern)
        {
            mcp_protocol::stamp(&mut params)?;
        }
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

    pub async fn notify(&mut self, method: &str, mut params: Value) -> Result<()> {
        if self
            .connection_info
            .as_ref()
            .is_some_and(|info| info.era == Era::Modern)
        {
            mcp_protocol::stamp(&mut params)?;
        }
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

    fn discovery_unsupported(error: &anyhow::Error) -> bool {
        error.downcast_ref::<McpRpcError>().is_some_and(|error| {
            mcp_protocol::discovery_unsupported(error.code, error.data.as_ref())
        })
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

#[cfg(test)]
#[path = "mcp_client_protocol_tests.rs"]
mod protocol_tests;
