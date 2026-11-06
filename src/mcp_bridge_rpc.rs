use serde_json::{Map, Value, json};

use super::{BridgeFailure, BridgeState};

use crate::mcp_protocol;
pub(super) use crate::mcp_protocol::MODERN_VERSION as MODERN_PROTOCOL_VERSION;

use crate::mcp_protocol::{SERVER_INFO_META_KEY, SUBSCRIPTION_ID_META_KEY};

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
        "tools/list" => state.list_tools(only_server).await.map(|tools| {
            Value::Object(Map::from_iter([("tools".to_owned(), Value::Array(tools))]))
        }),
        "tools/call" => call_tool(state, request.params, only_server).await,
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
    mcp_protocol::supports_version(version)
}

pub(super) fn modern_protocol_version(message: &Value) -> Option<&str> {
    mcp_protocol::protocol_metadata(message.get("params")?)
}

pub(super) fn listen_subscription(
    message: Value,
) -> std::result::Result<ListenSubscription, Value> {
    let request = match parse_request(message) {
        Ok(request) => request,
        Err((id, code, message)) => return Err(error_response(id, code, message)),
    };
    let Some(id) = request.id else {
        return Err(error_response(
            Value::Null,
            -32600,
            "Subscription requests must include an id.",
        ));
    };
    let Some(params) = request.params.as_object() else {
        return Err(error_response(id, -32602, "Invalid MCP bridge parameters."));
    };
    let Some(notifications) = params.get("notifications").and_then(Value::as_object) else {
        return Err(error_response(
            id,
            -32602,
            "Invalid MCP notification filter.",
        ));
    };
    if notifications.values().any(|value| !value.is_boolean()) {
        return Err(error_response(
            id,
            -32602,
            "Invalid MCP notification filter.",
        ));
    }
    let tools_list_changed = notifications
        .get("toolsListChanged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(ListenSubscription {
        id,
        tools_list_changed,
    })
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
        "_meta":{(SUBSCRIPTION_ID_META_KEY):id}
    })
}

pub(super) fn subscription_completion(id: &Value) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "result":{
            "resultType":"complete",
            "_meta":{
                (SUBSCRIPTION_ID_META_KEY):id,
                (SERVER_INFO_META_KEY):{
                    "name":"mcp-cli",
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
    let Value::Object(mut object) = message else {
        return Err((Value::Null, -32600, "Invalid JSON-RPC request."));
    };
    let id = match object.remove("id") {
        None | Some(Value::Null) => None,
        Some(value @ (Value::String(_) | Value::Number(_))) => Some(value),
        Some(_) => return Err((Value::Null, -32600, "Invalid JSON-RPC request id.")),
    };
    if object.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
        return Err((
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid JSON-RPC version.",
        ));
    }
    let Some(Value::String(method)) = object.remove("method") else {
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
    let params = object.remove("params").unwrap_or_else(|| json!({}));
    if !params.is_object() && !params.is_array() {
        return Err((
            id.unwrap_or(Value::Null),
            -32600,
            "Invalid JSON-RPC parameters.",
        ));
    }
    Ok(Request { id, method, params })
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
        Some(server) => format!("mcp-cli bridge exposing the '{server}' server."),
        None => {
            "mcp-cli bridge exposing configured keep-alive servers. Tool names are namespaced as server__tool."
                .to_owned()
        }
    };
    Ok(json!({
        "protocolVersion": negotiate_protocol_version(requested_version),
        "capabilities": {"tools": {}},
        "serverInfo": {"name":"mcp-cli", "version":env!("CARGO_PKG_VERSION")},
        "instructions": instructions,
    }))
}

fn discover_result(
    only_server: Option<&str>,
    supports_listen: bool,
) -> std::result::Result<Value, BridgeFailure> {
    let instructions = match only_server {
        Some(server) => format!("mcp-cli bridge exposing the '{server}' server."),
        None => {
            "mcp-cli bridge exposing configured keep-alive servers. Tool names are namespaced as server__tool."
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
            (SERVER_INFO_META_KEY):{
                "name":"mcp-cli",
                "version":env!("CARGO_PKG_VERSION")
            }
        }
    }))
}

async fn call_tool(
    state: &BridgeState,
    params: Value,
    only_server: Option<&str>,
) -> std::result::Result<Value, BridgeFailure> {
    let Value::Object(mut params) = params else {
        return Err(BridgeFailure::invalid_params());
    };
    let Some(Value::String(name)) = params.remove("name") else {
        return Err(BridgeFailure::invalid_params());
    };
    let arguments = params
        .remove("arguments")
        .unwrap_or_else(|| Value::Object(Map::new()));
    if !arguments.is_object() {
        return Err(BridgeFailure::invalid_params());
    }
    state.call_tool(&name, arguments, only_server).await
}

fn negotiate_protocol_version(requested: &str) -> String {
    mcp_protocol::negotiate_legacy_version(requested).to_owned()
}

fn success_response(id: Value, result: Value) -> Value {
    Value::Object(Map::from_iter([
        ("jsonrpc".to_owned(), Value::from("2.0")),
        ("id".to_owned(), id),
        ("result".to_owned(), result),
    ]))
}

fn error_response(id: Value, code: i64, message: &str) -> Value {
    Value::Object(Map::from_iter([
        ("jsonrpc".to_owned(), Value::from("2.0")),
        ("id".to_owned(), id),
        ("error".to_owned(), json!({"code":code, "message":message})),
    ]))
}
