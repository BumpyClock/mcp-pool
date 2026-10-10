use serde_json::{Map, Value, json};

use super::{BridgeFailure, BridgeState};

const DEFAULT_PROTOCOL_VERSION: &str = "2025-11-25";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] =
    &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
pub(super) const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";
const PROTOCOL_VERSION_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";
const SUBSCRIPTION_ID_META_KEY: &str = "io.modelcontextprotocol/subscriptionId";

pub(super) struct ListenSubscription {
    pub id: Value,
    pub tools_list_changed: bool,
}

struct Request {
    id: Option<Value>,
    method: String,
    params: Value,
}

pub(super) async fn dispatch(
    state: &BridgeState,
    message: Value,
    only_server: Option<&str>,
) -> Option<Value> {
    dispatch_with_capabilities(state, message, only_server, false).await
}

pub(super) async fn dispatch_http(
    state: &BridgeState,
    message: Value,
    only_server: Option<&str>,
) -> Option<Value> {
    dispatch_with_capabilities(state, message, only_server, true).await
}

async fn dispatch_with_capabilities(
    state: &BridgeState,
    message: Value,
    only_server: Option<&str>,
    supports_listen: bool,
) -> Option<Value> {
    let modern_request = modern_protocol_version(&message) == Some(MODERN_PROTOCOL_VERSION);
    let request = match parse_request(message) {
        Ok(request) => request,
        Err((id, code, message)) => return Some(error_response(id, code, message)),
    };
    let id = request.id?;
    if request.method.starts_with("notifications/") {
        return Some(error_response(
            id,
            -32600,
            "Notifications must not include a request id.",
        ));
    }

    let result = match request.method.as_str() {
        "initialize" => initialize_result(&request.params, only_server),
        "server/discover" if modern_request => discover_result(only_server, supports_listen),
        "ping" => Ok(json!({})),
        "tools/list" => state
            .list_tools(only_server, only_server.is_some())
            .await
            .map(|tools| json!({"tools":tools})),
        "tools/call" => call_tool(state, &request.params, only_server).await,
        _ => {
            return Some(error_response(
                id,
                -32601,
                "MCP method is not supported by this bridge.",
            ));
        }
    };

    Some(match result {
        Ok(result) => success_response(id, result),
        Err(failure) => error_response(id, failure.code, failure.message),
    })
}

pub(super) fn protocol_version_for_request(message: &Value) -> Option<String> {
    let method = message.get("method").and_then(Value::as_str)?;
    if let Some(version) = modern_protocol_version(message) {
        return Some(version.to_owned());
    }
    if method != "initialize" {
        return None;
    }
    let requested = message
        .get("params")
        .and_then(|params| params.get("protocolVersion"))
        .and_then(Value::as_str)?;
    Some(negotiate_protocol_version(requested))
}

pub(super) fn supports_protocol_version(version: &str) -> bool {
    version == MODERN_PROTOCOL_VERSION || SUPPORTED_PROTOCOL_VERSIONS.contains(&version)
}

pub(super) fn modern_protocol_version(message: &Value) -> Option<&str> {
    message
        .get("params")?
        .get("_meta")?
        .get(PROTOCOL_VERSION_META_KEY)?
        .as_str()
}

pub(super) fn listen_subscription(
    message: &Value,
) -> Option<std::result::Result<ListenSubscription, Value>> {
    if message.get("method").and_then(Value::as_str) != Some("subscriptions/listen") {
        return None;
    }
    let request = match parse_request(message.clone()) {
        Ok(request) => request,
        Err((id, code, message)) => return Some(Err(error_response(id, code, message))),
    };
    let Some(id) = request.id else {
        return Some(Err(error_response(
            Value::Null,
            -32600,
            "Subscription requests must include an id.",
        )));
    };
    let Some(params) = request.params.as_object() else {
        return Some(Err(error_response(
            id,
            -32602,
            "Invalid MCP bridge parameters.",
        )));
    };
    let Some(notifications) = params.get("notifications").and_then(Value::as_object) else {
        return Some(Err(error_response(
            id,
            -32602,
            "Invalid MCP notification filter.",
        )));
    };
    if notifications.values().any(|value| !value.is_boolean()) {
        return Some(Err(error_response(
            id,
            -32602,
            "Invalid MCP notification filter.",
        )));
    }
    let tools_list_changed = notifications
        .get("toolsListChanged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Some(Ok(ListenSubscription {
        id,
        tools_list_changed,
    }))
}

pub(super) fn subscription_acknowledgement(id: &Value, tools_list_changed: bool) -> Value {
    let notifications = if tools_list_changed {
        json!({"toolsListChanged":true})
    } else {
        json!({})
    };
    json!({
        "jsonrpc":"2.0",
        "method":"notifications/subscriptions/acknowledged",
        "params":{"notifications":notifications},
        "_meta":{"io.modelcontextprotocol/subscriptionId":id}
    })
}

pub(super) fn subscription_completion(id: &Value) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "result":{
            "resultType":"complete",
            "_meta":{
                "io.modelcontextprotocol/subscriptionId":id,
                "io.modelcontextprotocol/serverInfo":{
                    "name":"mcp-pool",
                    "version":env!("CARGO_PKG_VERSION")
                }
            }
        }
    })
}

pub(super) fn server_failure_response(id: Value) -> Value {
    error_response(id, -32603, "MCP pool request failed.")
}

pub(super) fn is_tools_list_changed(message: &Value) -> bool {
    message.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
        && message.get("method").and_then(Value::as_str) == Some("notifications/tools/list_changed")
}

pub(super) fn stamp_subscription_id(message: &mut Value, id: &Value) -> bool {
    let Some(message) = message.as_object_mut() else {
        return false;
    };
    let metadata = message.entry("_meta").or_insert_with(|| json!({}));
    let Some(metadata) = metadata.as_object_mut() else {
        return false;
    };
    metadata.insert(SUBSCRIPTION_ID_META_KEY.to_owned(), id.clone());
    true
}

pub(super) fn malformed_frame_response(too_large: bool) -> Value {
    if too_large {
        error_response(Value::Null, -32600, "JSON-RPC message is too large.")
    } else {
        error_response(Value::Null, -32700, "Parse error.")
    }
}

fn parse_request(message: Value) -> Result<Request, (Value, i64, &'static str)> {
    let Some(object) = message.as_object() else {
        return Err((Value::Null, -32600, "Invalid JSON-RPC request."));
    };
    let id = match object.get("id") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) => Some(Value::String(value.clone())),
        Some(Value::Number(value)) => Some(Value::Number(value.clone())),
        Some(_) => return Err((Value::Null, -32600, "Invalid JSON-RPC request id.")),
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err((
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid JSON-RPC version.",
        ));
    }
    let Some(method) = object.get("method").and_then(Value::as_str) else {
        return Err((
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid JSON-RPC method.",
        ));
    };
    if method.is_empty() {
        return Err((
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid JSON-RPC method.",
        ));
    }
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    if !params.is_object() && !params.is_array() {
        return Err((
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid JSON-RPC parameters.",
        ));
    }
    Ok(Request {
        id,
        method: method.to_owned(),
        params,
    })
}

fn initialize_result(
    params: &Value,
    only_server: Option<&str>,
) -> std::result::Result<Value, BridgeFailure> {
    let Some(params) = params.as_object() else {
        return Err(BridgeFailure::invalid_params());
    };
    let Some(requested_version) = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .filter(|version| !version.is_empty())
    else {
        return Err(BridgeFailure::invalid_params());
    };
    if !params.get("capabilities").is_some_and(Value::is_object)
        || !params.get("clientInfo").is_some_and(Value::is_object)
    {
        return Err(BridgeFailure::invalid_params());
    }

    let instructions = match only_server {
        Some(server) => format!("mcp-pool bridge exposing the '{server}' server."),
        None => {
            "mcp-pool bridge exposing configured keep-alive servers. Tool names are namespaced as server__tool."
                .to_owned()
        }
    };
    Ok(json!({
        "protocolVersion": negotiate_protocol_version(requested_version),
        "capabilities": {"tools": {}},
        "serverInfo": {"name":"mcp-pool", "version":env!("CARGO_PKG_VERSION")},
        "instructions": instructions,
    }))
}

fn discover_result(
    only_server: Option<&str>,
    supports_listen: bool,
) -> std::result::Result<Value, BridgeFailure> {
    let instructions = match only_server {
        Some(server) => format!("mcp-pool bridge exposing the '{server}' server."),
        None => {
            "mcp-pool bridge exposing configured keep-alive servers. Tool names are namespaced as server__tool."
                .to_owned()
        }
    };
    let tool_capability = if supports_listen {
        json!({"listChanged":true})
    } else {
        json!({})
    };
    Ok(json!({
        "supportedVersions":[MODERN_PROTOCOL_VERSION],
        "capabilities":{"tools":tool_capability},
        "instructions":instructions,
        "_meta":{
            "io.modelcontextprotocol/serverInfo":{
                "name":"mcp-pool",
                "version":env!("CARGO_PKG_VERSION")
            }
        }
    }))
}

async fn call_tool(
    state: &BridgeState,
    params: &Value,
    only_server: Option<&str>,
) -> std::result::Result<Value, BridgeFailure> {
    let Some(params) = params.as_object() else {
        return Err(BridgeFailure::invalid_params());
    };
    let Some(name) = params.get("name").and_then(Value::as_str) else {
        return Err(BridgeFailure::invalid_params());
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    if !arguments.is_object() {
        return Err(BridgeFailure::invalid_params());
    }
    state.call_tool(name, arguments, only_server).await
}

fn negotiate_protocol_version(requested: &str) -> String {
    if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested.to_owned()
    } else {
        DEFAULT_PROTOCOL_VERSION.to_owned()
    }
}

fn success_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "result":result})
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "error":{"code":code, "message":message}
    })
}
