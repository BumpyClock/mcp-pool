use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::http::header::{ACCEPT, ALLOW, CONTENT_TYPE, HOST, HeaderValue};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use reqwest::Url;
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tokio::time::timeout;

use super::{BridgeState, rpc};
#[path = "mcp_bridge_http_sse.rs"]
mod sse;

const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_CONCURRENT_REQUESTS: usize = 64;
const MAX_NOTIFICATION_LISTENERS: usize = 64;
#[cfg(not(test))]
const REQUEST_BODY_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const REQUEST_BODY_TIMEOUT: Duration = Duration::from_millis(50);
const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const SESSION_ID_HEADER: &str = "mcp-session-id";
const METHOD_HEADER: &str = "mcp-method";
const NAME_HEADER: &str = "mcp-name";

#[cfg(test)]
#[path = "mcp_bridge_http_tests.rs"]
mod tests;

#[derive(Clone)]
struct HttpState {
    bridge: Arc<BridgeState>,
    bind_host: String,
    port: u16,
    requests: Arc<Semaphore>,
    notification_listeners: Arc<Semaphore>,
    shutdown: watch::Sender<bool>,
}

enum Endpoint {
    Aggregate,
    Single(String),
    BadPath,
    NotFound,
}

pub(super) async fn serve(state: Arc<BridgeState>, host: String, port: u16) -> Result<()> {
    let listener = TcpListener::bind((host.as_str(), port)).await?;
    let address = listener.local_addr()?;
    let (shutdown, _) = watch::channel(false);
    let shutdown_signal = shutdown.clone();
    let application = Router::new()
        .fallback(handle_request)
        .with_state(HttpState {
            bridge: state,
            bind_host: host,
            port: address.port(),
            requests: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            notification_listeners: Arc::new(Semaphore::new(MAX_NOTIFICATION_LISTENERS)),
            shutdown,
        });
    eprintln!("mcp-pool MCP bridge listening at http://{address}/mcp");
    axum::serve(listener, application)
        .with_graceful_shutdown(async move {
            wait_for_shutdown().await;
            shutdown_signal.send_replace(true);
        })
        .await?;
    Ok(())
}

async fn wait_for_shutdown() {
    if tokio::signal::ctrl_c().await.is_err() {
        eprintln!("mcp-pool MCP bridge could not register Ctrl-C shutdown.");
    }
}

async fn handle_request(State(state): State<HttpState>, request: Request) -> Response {
    if *state.shutdown.borrow() {
        return text_response(StatusCode::SERVICE_UNAVAILABLE, "Server is shutting down");
    }
    let endpoint = endpoint_for_path(request.uri().path());
    let only_server = match endpoint {
        Endpoint::Aggregate => None,
        Endpoint::Single(name) => {
            if !state.bridge.has_server(&name) {
                return text_response(StatusCode::NOT_FOUND, &format!("Unknown server '{name}'"));
            }
            Some(name)
        }
        Endpoint::BadPath => return text_response(StatusCode::BAD_REQUEST, "Bad request"),
        Endpoint::NotFound => return text_response(StatusCode::NOT_FOUND, "Not found"),
    };

    if request.method() != Method::POST {
        let mut response = text_response(StatusCode::METHOD_NOT_ALLOWED, "Method not allowed");
        response
            .headers_mut()
            .insert(ALLOW, HeaderValue::from_static("POST"));
        return response;
    }
    if !validate_origin_and_host(
        request.headers(),
        request.uri(),
        &state.bind_host,
        state.port,
    ) {
        return text_response(StatusCode::FORBIDDEN, "Forbidden");
    }
    let Ok(request_permit) = Arc::clone(&state.requests).try_acquire_owned() else {
        return text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Too many concurrent MCP requests",
        );
    };
    if request.headers().contains_key(SESSION_ID_HEADER) {
        return text_response(StatusCode::NOT_FOUND, "Unknown MCP session");
    }
    if !has_json_content_type(request.headers()) {
        return text_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected application/json",
        );
    }
    let headers = request.headers().clone();
    let request_version = match headers.get(PROTOCOL_VERSION_HEADER) {
        Some(value) => match value.to_str() {
            Ok(version) if rpc::supports_protocol_version(version) => Some(version.to_owned()),
            _ => return text_response(StatusCode::BAD_REQUEST, "Unsupported MCP protocol version"),
        },
        None => None,
    };
    let modern_transport_headers = request_version.as_deref() == Some(rpc::MODERN_PROTOCOL_VERSION)
        || headers.contains_key(METHOD_HEADER)
        || headers.contains_key(NAME_HEADER);
    if !modern_transport_headers && !accepts_streamable_json(&headers) {
        return text_response(
            StatusCode::NOT_ACCEPTABLE,
            "Accept must include application/json and text/event-stream",
        );
    }
    let body = match timeout(
        REQUEST_BODY_TIMEOUT,
        to_bytes(request.into_body(), MAX_REQUEST_BYTES),
    )
    .await
    {
        Err(_) => return text_response(StatusCode::REQUEST_TIMEOUT, "Request body timed out"),
        Ok(Ok(body)) => body,
        Ok(Err(_)) => {
            return text_response(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large");
        }
    };
    let message = match serde_json::from_slice::<Value>(&body) {
        Ok(message) => message,
        Err(_) => return text_response(StatusCode::BAD_REQUEST, "Invalid JSON-RPC body"),
    };
    let body_version = rpc::modern_protocol_version(&message);
    let method = message.get("method").and_then(Value::as_str);
    if let Some(body_version) = body_version {
        if body_version != rpc::MODERN_PROTOCOL_VERSION
            || request_version.as_deref() != Some(body_version)
            || !valid_modern_headers(&headers, &message)
        {
            return text_response(
                StatusCode::BAD_REQUEST,
                "Invalid modern MCP request headers",
            );
        }
    } else if request_version.as_deref() == Some(rpc::MODERN_PROTOCOL_VERSION)
        || headers.contains_key(METHOD_HEADER)
        || headers.contains_key(NAME_HEADER)
    {
        return text_response(
            StatusCode::BAD_REQUEST,
            "Missing modern MCP request metadata",
        );
    }

    let response_version = rpc::protocol_version_for_request(&message).or(request_version);
    let modern_listen = body_version == Some(rpc::MODERN_PROTOCOL_VERSION)
        && method == Some("subscriptions/listen");
    if modern_listen {
        if !accepts_event_stream(&headers) {
            return text_response(
                StatusCode::NOT_ACCEPTABLE,
                "Accept must include text/event-stream",
            );
        }
        match rpc::listen_subscription(&message) {
            Some(Ok(subscription)) => {
                let mut response =
                    sse::start(state, subscription, only_server.as_deref(), request_permit).await;
                set_protocol_version(&mut response, response_version.as_deref());
                return response;
            }
            Some(Err(response)) => return json_response(response, response_version.as_deref()),
            None => {}
        }
    } else if modern_transport_headers && !accepts_streamable_json(&headers) {
        return text_response(
            StatusCode::NOT_ACCEPTABLE,
            "Accept must include application/json and text/event-stream",
        );
    }

    let response = rpc::dispatch_http(&state.bridge, message, only_server.as_deref()).await;
    match response {
        Some(response) => json_response(response, response_version.as_deref()),
        None => accepted_response(response_version.as_deref()),
    }
}

fn endpoint_for_path(path: &str) -> Endpoint {
    if path == "/mcp" {
        return Endpoint::Aggregate;
    }
    let Some(encoded_name) = path.strip_prefix("/mcp/") else {
        return Endpoint::NotFound;
    };
    match percent_decode(encoded_name) {
        Some(name) => Endpoint::Single(name),
        None => Endpoint::BadPath,
    }
}

fn percent_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut offset = 0;
    while let Some(byte) = bytes.get(offset) {
        if *byte != b'%' {
            decoded.push(*byte);
            offset += 1;
            continue;
        }
        let high = hex_value(*bytes.get(offset + 1)?)?;
        let low = hex_value(*bytes.get(offset + 2)?)?;
        decoded.push((high << 4) | low);
        offset += 3;
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn accepts_streamable_json(headers: &HeaderMap) -> bool {
    let (accepts_json, accepts_sse) = accepted_media_types(headers);
    accepts_json && accepts_sse
}

fn accepts_event_stream(headers: &HeaderMap) -> bool {
    accepted_media_types(headers).1
}

fn accepted_media_types(headers: &HeaderMap) -> (bool, bool) {
    let Some(value) = headers.get(ACCEPT).and_then(|value| value.to_str().ok()) else {
        return (false, false);
    };
    let mut accepts_json = false;
    let mut accepts_sse = false;
    for item in value.split(',') {
        let mut parts = item.split(';');
        let media_type = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        let quality_zero = parts.any(|parameter| {
            parameter.trim().strip_prefix("q=").is_some_and(|quality| {
                quality
                    .trim()
                    .parse::<f32>()
                    .is_ok_and(|value| value == 0.0)
            })
        });
        if quality_zero {
            continue;
        }
        accepts_json |= matches!(
            media_type.as_str(),
            "application/json" | "application/*" | "*/*"
        );
        accepts_sse |= matches!(media_type.as_str(), "text/event-stream" | "text/*" | "*/*");
    }
    (accepts_json, accepts_sse)
}

fn valid_modern_headers(headers: &HeaderMap, message: &Value) -> bool {
    let Some(method) = message.get("method").and_then(Value::as_str) else {
        return false;
    };
    if headers
        .get(METHOD_HEADER)
        .and_then(|value| value.to_str().ok())
        != Some(method)
    {
        return false;
    }
    match message.get("params").and_then(|params| params.get("name")) {
        Some(Value::String(name)) => {
            headers
                .get(NAME_HEADER)
                .and_then(|value| value.to_str().ok())
                == Some(name)
        }
        Some(_) => false,
        None => !headers.contains_key(NAME_HEADER),
    }
}

fn validate_origin_and_host(headers: &HeaderMap, uri: &Uri, bind_host: &str, port: u16) -> bool {
    if uri
        .scheme_str()
        .is_some_and(|scheme| !scheme.eq_ignore_ascii_case("http"))
    {
        return false;
    }
    let Some(host_header) = headers.get(HOST).and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let Some((request_host, request_port)) = parse_authority(host_header, "http") else {
        return false;
    };
    if request_port != port || !host_is_allowed(&request_host, bind_host) {
        return false;
    }
    if let Some(authority) = uri.authority() {
        let Some((uri_host, uri_port)) = parse_authority(authority.as_str(), "http") else {
            return false;
        };
        if !uri_host.eq_ignore_ascii_case(&request_host) || uri_port != request_port {
            return false;
        }
    }

    let Some(origin) = headers.get("origin") else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    if origin == "null" {
        return false;
    }
    let Ok(origin) = Url::parse(origin) else {
        return false;
    };
    if origin.scheme() != "http"
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return false;
    }
    let Some(origin_host) = origin.host_str() else {
        return false;
    };
    origin_host.eq_ignore_ascii_case(&request_host)
        && origin.port_or_known_default() == Some(request_port)
        && host_is_allowed(origin_host, bind_host)
}

fn parse_authority(authority: &str, scheme: &str) -> Option<(String, u16)> {
    let url = Url::parse(&format!("{scheme}://{authority}/")).ok()?;
    if !url.username().is_empty() || url.password().is_some() || url.path() != "/" {
        return None;
    }
    Some((url.host_str()?.to_owned(), url.port_or_known_default()?))
}

fn host_is_allowed(request_host: &str, bind_host: &str) -> bool {
    let request_host = request_host.trim_matches(['[', ']']);
    let bind_host = bind_host.trim_matches(['[', ']']);
    if matches!(bind_host, "0.0.0.0" | "::") {
        return true;
    }
    if is_loopback_host(bind_host) {
        return is_loopback_host(request_host);
    }
    request_host.eq_ignore_ascii_case(bind_host)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || IpAddr::from_str(host).is_ok_and(|address| address.is_loopback())
}

fn json_response(value: Value, protocol_version: Option<&str>) -> Response {
    let mut response = Response::new(Body::from(value.to_string()));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    set_protocol_version(&mut response, protocol_version);
    response
}

fn accepted_response(protocol_version: Option<&str>) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::ACCEPTED;
    set_protocol_version(&mut response, protocol_version);
    response
}

fn set_protocol_version(response: &mut Response, protocol_version: Option<&str>) {
    if let Some(protocol_version) = protocol_version
        && let Ok(value) = HeaderValue::from_str(protocol_version)
    {
        response
            .headers_mut()
            .insert(PROTOCOL_VERSION_HEADER, value);
    }
}

fn text_response(status: StatusCode, message: &str) -> Response {
    let mut response = Response::new(Body::from(message.to_owned()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
