use std::convert::Infallible;
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, mpsc, oneshot, watch};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::{Endpoint, HttpState, endpoint_for_path, handle_request};
use crate::mcp_bridge::{BridgeClient, BridgeState, ClientFuture};

#[path = "mcp_bridge_http_modern_tests.rs"]
mod modern;

struct HttpFixtureClient;

impl BridgeClient for HttpFixtureClient {
    fn is_closed(&self) -> bool {
        false
    }

    fn list_tools(&mut self) -> ClientFuture<'_, Vec<Value>> {
        Box::pin(async {
            Ok(vec![json!({
                "name":"ping",
                "description":"fixture ping",
                "inputSchema":{"type":"object","properties":{"value":{"type":"string"}}}
            })])
        })
    }

    fn request<'a>(&'a mut self, _method: &'a str, params: Value) -> ClientFuture<'a, Value> {
        Box::pin(async move {
            Ok(json!({
                "content":[{"type":"text","text":"fixture"}],
                "echo":params
            }))
        })
    }
}

#[test]
fn paths_route_aggregate_and_decode_single_server_names() {
    assert!(matches!(endpoint_for_path("/mcp"), Endpoint::Aggregate));
    assert!(matches!(
        endpoint_for_path("/mcp/alpha%2Dbeta"),
        Endpoint::Single(name) if name == "alpha-beta"
    ));
    assert!(matches!(
        endpoint_for_path("/mcp/%E0%A4%A"),
        Endpoint::BadPath
    ));
    assert!(matches!(
        endpoint_for_path("/mcp-extra"),
        Endpoint::NotFound
    ));
}

#[test]
fn accept_headers_preserve_media_ranges_and_zero_quality_exclusion() -> Result<()> {
    for (value, streamable_json, event_stream) in [
        ("application/json, text/event-stream", true, true),
        ("application/*, text/*", true, true),
        ("*/*", true, true),
        (" APPLICATION/JSON ; q=0.5, TEXT/EVENT-STREAM ", true, true),
        ("application/json;q=0, text/event-stream", false, true),
        ("application/json, text/event-stream;q=0.0", false, false),
        ("text/event-stream;q=0, text/*;q=1", false, true),
        ("application/json", false, false),
        ("text/event-stream", false, true),
        ("application/json;q=invalid, text/event-stream", true, true),
    ] {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("accept", value.parse()?);
        assert_eq!(
            super::accepts_streamable_json(&headers),
            streamable_json,
            "{value}"
        );
        assert_eq!(
            super::accepts_event_stream(&headers),
            event_stream,
            "{value}"
        );
    }
    let headers = axum::http::HeaderMap::new();
    assert!(!super::accepts_streamable_json(&headers));
    assert!(!super::accepts_event_stream(&headers));
    Ok(())
}

#[tokio::test]
async fn malformed_json_and_unknown_routes_return_client_errors() -> Result<()> {
    let state = state()?;
    let invalid = request("/mcp", "{not-json}")?;
    let invalid_response = handle_request(State(http_state(state.clone())), invalid).await;
    assert_eq!(invalid_response.status(), StatusCode::BAD_REQUEST);

    let unknown = request("/not-mcp", "{}")?;
    let unknown_response = handle_request(State(http_state(state.clone())), unknown).await;
    assert_eq!(unknown_response.status(), StatusCode::NOT_FOUND);

    let unknown_server = request("/mcp/missing", "{}")?;
    let unknown_server_response =
        handle_request(State(http_state(state.clone())), unknown_server).await;
    assert_eq!(unknown_server_response.status(), StatusCode::NOT_FOUND);
    let malformed_path = request("/mcp/%E0%A4%A", "{}")?;
    let malformed_path_response = handle_request(State(http_state(state)), malformed_path).await;
    assert_eq!(malformed_path_response.status(), StatusCode::BAD_REQUEST);
    Ok(())
}

#[tokio::test]
async fn per_server_http_endpoint_returns_bare_tool_names() -> Result<()> {
    let state = state()?;
    let request = request(
        "/mcp/alpha",
        r#"{"jsonrpc":"2.0","id":"list-1","method":"tools/list","params":{}}"#,
    )?;
    let response = handle_request(State(http_state(state)), request).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("application/json")
    );
    assert!(!response.headers().contains_key("mcp-session-id"));
    let body = to_bytes(response.into_body(), 4096).await?;
    let value: Value = serde_json::from_slice(&body)?;
    assert_eq!(value.get("id").and_then(Value::as_str), Some("list-1"));
    assert_eq!(
        value
            .get("result")
            .and_then(|result| result.get("tools"))
            .and_then(Value::as_array)
            .and_then(|tools| tools.first())
            .and_then(|tool| tool.get("name"))
            .and_then(Value::as_str),
        Some("ping")
    );
    Ok(())
}

#[tokio::test]
async fn streamable_http_round_trip_preserves_ids_and_protocol_version() -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let application = Router::new()
        .fallback(handle_request)
        .with_state(http_state_at(state()?, address.port()));
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, application)
            .with_graceful_shutdown(async { if shutdown_receiver.await.is_err() {} })
            .await
    });
    let client = reqwest::Client::new();
    let endpoint = format!("http://{address}/mcp");
    let accept = "application/json, text/event-stream";

    let initialized = client
        .post(&endpoint)
        .header("accept", accept)
        .json(&json!({
            "jsonrpc":"2.0",
            "id":"init-http",
            "method":"initialize",
            "params":{
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"fixture","version":"1"}
            }
        }))
        .send()
        .await?;
    let initialize_status = initialized.status().as_u16();
    let initialize_version = initialized
        .headers()
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let initialize_body = initialized.json::<Value>().await?;

    let listed = client
        .post(&endpoint)
        .header("accept", accept)
        .header("mcp-protocol-version", "2025-11-25")
        .json(&json!({
            "jsonrpc":"2.0",
            "id":22,
            "method":"tools/list",
            "params":{}
        }))
        .send()
        .await?;
    let list_status = listed.status().as_u16();
    let list_body = listed.json::<Value>().await?;

    let called = client
        .post(&endpoint)
        .header("accept", accept)
        .header("mcp-protocol-version", "2025-11-25")
        .json(&json!({
            "jsonrpc":"2.0",
            "id":"call-http",
            "method":"tools/call",
            "params":{"name":"alpha__ping","arguments":{"value":"x"}}
        }))
        .send()
        .await?;
    let call_status = called.status().as_u16();
    let call_body = called.json::<Value>().await?;

    shutdown_sender
        .send(())
        .map_err(|_| anyhow::anyhow!("HTTP fixture server stopped early."))?;
    server.await??;

    assert_eq!(initialize_status, 200);
    assert_eq!(initialize_version.as_deref(), Some("2025-11-25"));
    assert_eq!(
        initialize_body.get("id").and_then(Value::as_str),
        Some("init-http")
    );
    assert_eq!(list_status, 200);
    assert_eq!(list_body.get("id").and_then(Value::as_i64), Some(22));
    assert_eq!(
        list_body
            .get("result")
            .and_then(|result| result.get("tools"))
            .and_then(Value::as_array)
            .and_then(|tools| tools.first())
            .and_then(|tool| tool.get("name"))
            .and_then(Value::as_str),
        Some("alpha__ping")
    );
    assert_eq!(call_status, 200);
    assert_eq!(
        call_body.get("id").and_then(Value::as_str),
        Some("call-http")
    );
    assert_eq!(
        call_body
            .get("result")
            .and_then(|result| result.get("echo"))
            .and_then(|echo| echo.get("arguments"))
            .and_then(|arguments| arguments.get("value"))
            .and_then(Value::as_str),
        Some("x")
    );
    Ok(())
}

#[tokio::test]
async fn loopback_origin_is_allowed_and_external_origin_is_rejected() -> Result<()> {
    let state = state()?;
    let mut denied = request(
        "/mcp",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
    )?;
    denied
        .headers_mut()
        .insert("origin", "http://attacker.example:3000".parse()?);
    let denied_response = handle_request(State(http_state(state.clone())), denied).await;
    assert_eq!(denied_response.status(), StatusCode::FORBIDDEN);

    let mut mismatched_authority = request("http://attacker.example:3000/mcp", "{}")?;
    mismatched_authority
        .headers_mut()
        .insert("origin", "http://127.0.0.1:3000".parse()?);
    let authority_response =
        handle_request(State(http_state(state.clone())), mismatched_authority).await;
    assert_eq!(authority_response.status(), StatusCode::FORBIDDEN);

    let mut secure_origin = request("/mcp", "{}")?;
    secure_origin
        .headers_mut()
        .insert("origin", "https://127.0.0.1:3000".parse()?);
    let secure_response = handle_request(State(http_state(state.clone())), secure_origin).await;
    assert_eq!(secure_response.status(), StatusCode::FORBIDDEN);

    let mut accepted = request(
        "/mcp",
        r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#,
    )?;
    accepted
        .headers_mut()
        .insert("origin", "http://127.0.0.1:3000".parse()?);
    let accepted_response = handle_request(State(http_state(state)), accepted).await;
    assert_eq!(accepted_response.status(), StatusCode::OK);
    assert_eq!(
        accepted_response
            .headers()
            .get("mcp-protocol-version")
            .and_then(|value| value.to_str().ok()),
        Some("2025-11-25")
    );
    Ok(())
}

#[tokio::test]
async fn stateless_http_rejects_session_ids_and_requires_streamable_headers() -> Result<()> {
    let state = state()?;
    let mut session = request("/mcp", "{}")?;
    session
        .headers_mut()
        .insert("mcp-session-id", "unknown-session".parse()?);
    let session_response = handle_request(State(http_state(state.clone())), session).await;
    assert_eq!(session_response.status(), StatusCode::NOT_FOUND);

    let mut missing_sse = request("/mcp", "{}")?;
    missing_sse
        .headers_mut()
        .insert("accept", "application/json".parse()?);
    let accept_response = handle_request(State(http_state(state)), missing_sse).await;
    assert_eq!(accept_response.status(), StatusCode::NOT_ACCEPTABLE);
    Ok(())
}

#[tokio::test]
async fn http_rejects_oversized_requests_and_unsupported_methods() -> Result<()> {
    let state = state()?;
    let large_body = " ".repeat(super::MAX_REQUEST_BYTES + 1);
    let oversized = request("/mcp", &large_body)?;
    let oversized_response = handle_request(State(http_state(state.clone())), oversized).await;
    assert_eq!(oversized_response.status(), StatusCode::PAYLOAD_TOO_LARGE);

    let mut get = request("/mcp", "{}")?;
    *get.method_mut() = Method::GET;
    let method_response = handle_request(State(http_state(state)), get).await;
    assert_eq!(method_response.status(), StatusCode::METHOD_NOT_ALLOWED);
    Ok(())
}

#[tokio::test]
async fn http_bounds_request_body_read_time() -> Result<()> {
    let state = state()?;
    let (sender, receiver) = mpsc::channel::<Vec<u8>>(1);
    let body = Body::from_stream(ReceiverStream::new(receiver).map(Ok::<_, Infallible>));
    let request = Request::builder()
        .method(Method::POST)
        .uri("/mcp")
        .header("host", "127.0.0.1:3000")
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body)?;
    let response = handle_request(State(http_state(state)), request).await;
    drop(sender);
    assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    Ok(())
}

fn state() -> Result<Arc<BridgeState>> {
    Ok(Arc::new(BridgeState::with_clients(vec![(
        "alpha".to_owned(),
        Box::new(HttpFixtureClient),
    )])?))
}

fn http_state(bridge: Arc<BridgeState>) -> HttpState {
    http_state_at(bridge, 3000)
}

fn http_state_at(bridge: Arc<BridgeState>, port: u16) -> HttpState {
    let (shutdown, _) = watch::channel(false);
    HttpState {
        bridge,
        bind_host: "127.0.0.1".to_owned(),
        port,
        requests: Arc::new(Semaphore::new(super::MAX_CONCURRENT_REQUESTS)),
        notification_listeners: Arc::new(Semaphore::new(super::MAX_NOTIFICATION_LISTENERS)),
        shutdown,
    }
}

fn request(path: &str, body: &str) -> Result<Request<Body>> {
    Request::builder()
        .method(Method::POST)
        .uri(path)
        .header("host", "127.0.0.1:3000")
        .header("content-type", "application/json; charset=utf-8")
        .header("accept", "application/json, text/event-stream")
        .body(Body::from(body.to_owned()))
        .map_err(|error| anyhow::anyhow!(error))
}
