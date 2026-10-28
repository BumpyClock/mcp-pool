use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::{Value, json};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use crate::mcp_client::McpClient;
use crate::server_config::{ConfiguredServer, ServerConfiguration};

use crate::tool_filter as filter;
#[path = "mcp_bridge_http.rs"]
mod http;
#[path = "mcp_bridge_names.rs"]
mod names;
#[path = "mcp_bridge_options.rs"]
mod options;
#[path = "mcp_bridge_rpc.rs"]
mod rpc;
#[path = "mcp_bridge_stdio.rs"]
mod stdio;
#[cfg(test)]
#[path = "mcp_bridge_test_fixture.rs"]
mod test_fixture;
#[cfg(test)]
#[path = "mcp_bridge_tests.rs"]
mod tests;

const DEFAULT_TIMEOUT_MS: u64 = 60_000;

type ClientFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;
type ReconnectFactory = Arc<dyn Fn() -> ClientFuture<'static, McpClient> + Send + Sync>;

type SharedClient = Arc<Mutex<McpClient>>;

struct ServerClient {
    client: SharedClient,
    reconnect: ReconnectFactory,
    filter: filter::ToolFilter,
    timeout: Duration,
}

struct BridgeState {
    server_order: Vec<String>,
    clients: BTreeMap<String, ServerClient>,
}

#[derive(Clone, Copy)]
pub(super) struct BridgeFailure {
    pub code: i64,
    pub message: &'static str,
}

impl BridgeFailure {
    fn internal() -> Self {
        Self {
            code: -32603,
            message: "MCP pool request failed.",
        }
    }

    fn invalid_params() -> Self {
        Self {
            code: -32602,
            message: "Invalid MCP bridge parameters.",
        }
    }

    fn unknown_server() -> Self {
        Self {
            code: -32602,
            message: "Unknown bridged MCP server.",
        }
    }

    fn unknown_tool() -> Self {
        Self {
            code: -32602,
            message: "Unknown bridged MCP tool.",
        }
    }
}

impl BridgeState {
    async fn connect(servers: Vec<ConfiguredServer>) -> Result<Self> {
        let servers = servers
            .into_iter()
            .map(|server| {
                let filter = server.tool_filter().map_err(|_| {
                    anyhow::anyhow!("Server '{}' has invalid tool filters.", server.name)
                })?;
                Ok((server, filter))
            })
            .collect::<Result<Vec<_>>>()?;
        let server_order = servers
            .iter()
            .map(|(server, _)| server.name.clone())
            .collect::<Vec<_>>();
        let mut connections = JoinSet::new();

        for (server, filter) in servers {
            connections.spawn(async move {
                let name = server.name.clone();
                let definition = crate::oauth::prepare(&server, false)
                    .await
                    .map_err(|_| anyhow::anyhow!("Could not prepare MCP server '{name}'."))?;
                let pool_name = crate::mcp_cli::pool_name(&server, &definition)
                    .map_err(|_| anyhow::anyhow!("Could not identify MCP server '{name}'."))?;
                let timeout = request_timeout(&definition);
                let reconnect =
                    pool_reconnect_factory(pool_name.clone(), definition.clone(), timeout);
                let client = McpClient::connect(&pool_name, &definition, timeout)
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("Could not connect to pooled MCP server '{name}'.")
                    })?;
                Ok::<_, anyhow::Error>((name, client, reconnect, filter, timeout))
            });
        }

        let mut clients = BTreeMap::new();
        while let Some(completed) = connections.join_next().await {
            match completed {
                Ok(Ok((name, client, reconnect, filter, timeout))) => {
                    clients.insert(
                        name,
                        ServerClient {
                            client: Arc::new(Mutex::new(client)),
                            reconnect,
                            filter,
                            timeout,
                        },
                    );
                }
                Ok(Err(error)) => {
                    connections.abort_all();
                    return Err(error);
                }
                Err(_) => {
                    connections.abort_all();
                    bail!("Could not initialize pooled MCP connections.");
                }
            }
        }

        if clients.len() != server_order.len() {
            bail!("Could not initialize pooled MCP connections.");
        }
        Ok(Self {
            server_order,
            clients,
        })
    }

    pub(super) fn has_server(&self, name: &str) -> bool {
        self.clients.contains_key(name)
    }

    pub(super) fn selected_server_names(
        &self,
        only_server: Option<&str>,
    ) -> std::result::Result<Vec<String>, BridgeFailure> {
        match only_server {
            Some(name) if self.has_server(name) => Ok(vec![name.to_owned()]),
            Some(_) => Err(BridgeFailure::unknown_server()),
            None => Ok(self.server_order.clone()),
        }
    }

    pub(super) async fn connect_notification_client(
        &self,
        server: &str,
    ) -> std::result::Result<McpClient, BridgeFailure> {
        let Some(server_client) = self.clients.get(server) else {
            return Err(BridgeFailure::unknown_server());
        };
        (server_client.reconnect)()
            .await
            .map_err(|_| BridgeFailure::internal())
    }

    pub(super) async fn list_tools(
        &self,
        only_server: Option<&str>,
    ) -> std::result::Result<Vec<Value>, BridgeFailure> {
        let names = self.selected_server_names(only_server)?;
        let mut pending = JoinSet::new();
        for name in &names {
            let Some(server_client) = self.clients.get(name) else {
                return Err(BridgeFailure::unknown_server());
            };
            let client = Arc::clone(&server_client.client);
            let reconnect = Arc::clone(&server_client.reconnect);
            let timeout = server_client.timeout;
            let server_name = name.clone();
            pending.spawn(async move {
                let result = tokio::time::timeout(timeout, async {
                    let mut client = client.lock().await;
                    reconnect_if_closed(&mut client, &reconnect).await?;
                    client.list_tools().await
                })
                .await;
                (server_name, result)
            });
        }

        let mut listed = BTreeMap::new();
        while let Some(completed) = pending.join_next().await {
            match completed {
                Ok((name, Ok(Ok(tools)))) => {
                    listed.insert(name, tools);
                }
                Ok((_, Ok(Err(_)))) | Ok((_, Err(_))) | Err(_) => {
                    return Err(BridgeFailure::internal());
                }
            }
        }

        let mut result = Vec::new();
        for server in names {
            let Some(tools) = listed.remove(&server) else {
                return Err(BridgeFailure::internal());
            };
            let Some(server_client) = self.clients.get(&server) else {
                return Err(BridgeFailure::unknown_server());
            };
            for tool in tools {
                let Some(name) = tool.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if server_client.filter.permits(name)
                    && let Some(tool) = expose_tool(&server, tool, only_server.is_some())
                {
                    result.push(tool);
                }
            }
        }
        Ok(result)
    }

    pub(super) async fn call_tool(
        &self,
        name: &str,
        arguments: Value,
        only_server: Option<&str>,
    ) -> std::result::Result<Value, BridgeFailure> {
        let (server, tool) = match only_server {
            Some(server) if self.has_server(server) && !name.is_empty() => {
                (server.to_owned(), name.to_owned())
            }
            Some(_) => return Err(BridgeFailure::unknown_tool()),
            None => match names::decode_tool_name(name, &self.server_order) {
                Some(decoded) => decoded,
                None => return Err(BridgeFailure::unknown_tool()),
            },
        };
        let Some(server_client) = self.clients.get(&server) else {
            return Err(BridgeFailure::unknown_server());
        };
        if !server_client.filter.permits(&tool) {
            return Err(BridgeFailure::unknown_tool());
        }
        tokio::time::timeout(server_client.timeout, async {
            let mut client = server_client.client.lock().await;
            reconnect_if_closed(&mut client, &server_client.reconnect).await?;
            client
                .request("tools/call", json!({"name":tool, "arguments":arguments}))
                .await
        })
        .await
        .map_err(|_| BridgeFailure::internal())?
        .map_err(|_| BridgeFailure::internal())
    }
}

async fn reconnect_if_closed(client: &mut McpClient, reconnect: &ReconnectFactory) -> Result<()> {
    if client.is_closed() {
        *client = reconnect().await?;
    }
    Ok(())
}

fn pool_reconnect_factory(
    pool_name: String,
    definition: crate::config::ServerDef,
    timeout: Duration,
) -> ReconnectFactory {
    Arc::new(move || {
        let pool_name = pool_name.clone();
        let definition = definition.clone();
        Box::pin(async move { McpClient::connect(&pool_name, &definition, timeout).await })
    })
}

fn request_timeout(definition: &crate::config::ServerDef) -> Duration {
    let milliseconds = definition
        .timeout_ms
        .filter(|timeout| *timeout > 0)
        .unwrap_or(DEFAULT_TIMEOUT_MS);
    Duration::from_millis(milliseconds)
}

fn expose_tool(server: &str, mut tool: Value, bare_names: bool) -> Option<Value> {
    let exposed = tool.as_object_mut()?;
    let name = exposed.get("name")?.as_str()?;
    if name.is_empty() {
        return None;
    }

    exposed.insert(
        "name".to_owned(),
        Value::String(if bare_names {
            name.to_owned()
        } else {
            names::encode_tool_name(server, name)
        }),
    );
    if !bare_names {
        let description = exposed.get("description").and_then(Value::as_str);
        exposed.insert(
            "description".to_owned(),
            Value::String(names::describe_tool(server, description)),
        );
    }
    if !exposed.get("inputSchema").is_some_and(|schema| {
        schema
            .as_object()
            .and_then(|schema| schema.get("type"))
            .and_then(Value::as_str)
            == Some("object")
    }) {
        exposed.insert("inputSchema".to_owned(), json!({"type":"object"}));
    }
    if !exposed.get("outputSchema").is_some_and(Value::is_object) {
        exposed.remove("outputSchema");
    }
    Some(tool)
}

pub async fn run(configuration: ServerConfiguration, arguments: Vec<String>) -> anyhow::Result<()> {
    let options = options::parse(arguments)?;
    let selected = options::select_servers(&configuration, options.servers.as_deref())?;
    let state = Arc::new(BridgeState::connect(selected).await?);
    match options.mode {
        options::ServeMode::Stdio => stdio::serve(state).await,
        options::ServeMode::Http { host, port } => http::serve(state, host, port).await,
    }
}
