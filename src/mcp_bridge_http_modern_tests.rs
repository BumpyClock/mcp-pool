use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use super::{handle_request, http_state_at};
use crate::mcp_bridge::{
    BridgeState,
    test_fixture::{self, Fixture},
};

const MODERN_VERSION: &str = "2026-07-28";
const ACCEPT: &str = "application/json, text/event-stream";

#[tokio::test]
async fn modern_http_negotiation_and_requests_follow_discovery_envelope() -> Result<()> {
    let state = super::state().await?;
    let (url, _shutdown, server) = start_http(state).await?;
    let client = reqwest::Client::new();
    let discovered = client
        .post(url.as_str())
        .header("accept", ACCEPT)
        .header("mcp-protocol-version", MODERN_VERSION)
        .header("mcp-method", "server/discover")
        .json(&json!({
            "jsonrpc":"2.0",
            "id":"discover-modern",
            "method":"server/discover",
            "params":{"_meta":modern_metadata()}
        }))
        .send()
        .await?;
    let discover_version = discovered
        .headers()
        .get("mcp-protocol-version")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let discover_body = discovered.json::<Value>().await?;

    let listed = client
        .post(url.as_str())
        .header("accept", ACCEPT)
        .header("mcp-protocol-version", MODERN_VERSION)
        .header("mcp-method", "tools/list")
        .json(&json!({
            "jsonrpc":"2.0",
            "id":"list-modern",
            "method":"tools/list",
            "params":{"_meta":modern_metadata()}
        }))
        .send()
        .await?;
    let list_body = listed.json::<Value>().await?;

    let called = client
        .post(url.as_str())
        .header("accept", ACCEPT)
        .header("mcp-protocol-version", MODERN_VERSION)
        .header("mcp-method", "tools/call")
        .header("mcp-name", "alpha__ping")
        .json(&json!({
            "jsonrpc":"2.0",
            "id":"call-modern",
            "method":"tools/call",
            "params":{
                "_meta":modern_metadata(),
                "name":"alpha__ping",
                "arguments":{"value":"v"}
            }
        }))
        .send()
        .await?;
    let call_body = called.json::<Value>().await?;

    assert_eq!(discover_version.as_deref(), Some(MODERN_VERSION));
    assert_eq!(
        discover_body.get("result"),
        Some(&json!({
            "supportedVersions":[MODERN_VERSION],
            "capabilities":{"tools":{"listChanged":true}},
            "instructions":"mcp-pool bridge exposing configured keep-alive servers. Tool names are namespaced as server__tool.",
            "_meta":{"io.modelcontextprotocol/serverInfo":{
                "name":"mcp-pool",
                "version":env!("CARGO_PKG_VERSION")
            }}
        }))
    );
    assert_eq!(
        list_body
            .pointer("/result/tools/0/name")
            .and_then(Value::as_str),
        Some("alpha__ping")
    );
    assert_eq!(
        list_body
            .pointer("/result/tools/0/inputSchema/properties/value/type")
            .and_then(Value::as_str),
        Some("string")
    );
    assert_eq!(
        call_body.get("id").and_then(Value::as_str),
        Some("call-modern")
    );
    assert_eq!(
        call_body
            .pointer("/result/echo/arguments/value")
            .and_then(Value::as_str),
        Some("v")
    );

    _shutdown.send_replace(true);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn modern_http_requires_matching_derived_method_and_name_headers() -> Result<()> {
    let state = super::state().await?;
    let (url, shutdown, server) = start_http(state).await?;
    let client = reqwest::Client::new();
    let response = client
        .post(url.as_str())
        .header("accept", ACCEPT)
        .header("mcp-protocol-version", MODERN_VERSION)
        .header("mcp-method", "tools/list")
        .json(&json!({
            "jsonrpc":"2.0",
            "id":"wrong-method",
            "method":"server/discover",
            "params":{"_meta":modern_metadata()}
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let missing_name = client
        .post(url.as_str())
        .header("accept", ACCEPT)
        .header("mcp-protocol-version", MODERN_VERSION)
        .header("mcp-method", "tools/call")
        .json(&json!({
            "jsonrpc":"2.0",
            "id":"missing-name",
            "method":"tools/call",
            "params":{"_meta":modern_metadata(),"name":"alpha__ping"}
        }))
        .send()
        .await?;
    assert_eq!(missing_name.status(), StatusCode::BAD_REQUEST);

    shutdown.send_replace(true);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn modern_listener_acknowledges_filters_and_streams_pool_notifications() -> Result<()> {
    let (state, fixture) = notification_state().await?;
    let (url, shutdown, server) = start_http(Arc::new(state)).await?;
    let client = reqwest::Client::new();
    let mut response = client
        .post(url.as_str())
        .header("accept", ACCEPT)
        .header("mcp-protocol-version", MODERN_VERSION)
        .header("mcp-method", "subscriptions/listen")
        .json(&listen_request("listen-one"))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream; charset=utf-8")
    );
    let acknowledgement = read_sse_message(&mut response).await?;
    assert_eq!(
        acknowledgement.get("method").and_then(Value::as_str),
        Some("notifications/subscriptions/acknowledged")
    );
    assert_eq!(
        acknowledgement
            .pointer("/params/notifications/toolsListChanged")
            .and_then(Value::as_bool),
        Some(true)
    );
    assert_eq!(
        acknowledgement
            .pointer("/_meta/io.modelcontextprotocol~1subscriptionId")
            .and_then(Value::as_str),
        Some("listen-one")
    );

    let keepalive = timeout(Duration::from_millis(500), response.chunk())
        .await
        .context("SSE keepalive was not sent")??
        .context("SSE stream ended before its keepalive")?;
    assert!(String::from_utf8_lossy(&keepalive).contains(": keepalive"));
    sleep(Duration::from_millis(350)).await;
    assert_eq!(fixture.connections(), 2);
    fixture.notify(json!({
        "jsonrpc":"2.0",
        "method":"notifications/resources/list_changed",
        "params":{}
    }))?;
    fixture.notify(json!({
        "jsonrpc":"2.0",
        "method":"notifications/tools/list_changed",
        "params":{}
    }))?;
    let changed = read_sse_message(&mut response).await?;
    assert_eq!(
        changed.get("method").and_then(Value::as_str),
        Some("notifications/tools/list_changed")
    );
    assert_eq!(
        changed
            .pointer("/_meta/io.modelcontextprotocol~1subscriptionId")
            .and_then(Value::as_str),
        Some("listen-one")
    );

    fixture.disconnect_listeners()?;
    fixture.wait_for_disconnect().await?;
    fixture.notify(json!({
        "jsonrpc":"2.0",
        "method":"notifications/tools/list_changed",
        "params":{"generation":"missed"}
    }))?;
    fixture.wait_for(3, 0, 0).await?;
    fixture.notify(json!({
        "jsonrpc":"2.0",
        "method":"notifications/tools/list_changed",
        "params":{"generation":2}
    }))?;
    let after_reconnect = read_sse_message(&mut response).await?;
    assert_eq!(
        after_reconnect.pointer("/params/generation"),
        Some(&json!(2))
    );
    assert_eq!(fixture.connections(), 3);

    drop(response);
    fixture.wait_for(3, 1, 0).await?;
    shutdown.send_replace(true);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn modern_listener_completes_before_graceful_http_shutdown() -> Result<()> {
    let (state, fixture) = notification_state().await?;
    let (url, shutdown, server) = start_http(Arc::new(state)).await?;
    let client = reqwest::Client::new();
    let mut response = client
        .post(url.as_str())
        .header("accept", ACCEPT)
        .header("mcp-protocol-version", MODERN_VERSION)
        .header("mcp-method", "subscriptions/listen")
        .json(&listen_request("listen-close"))
        .send()
        .await?;
    let acknowledgement = read_sse_message(&mut response).await?;
    assert_eq!(
        acknowledgement.get("method").and_then(Value::as_str),
        Some("notifications/subscriptions/acknowledged")
    );
    assert_eq!(fixture.connections(), 2);

    shutdown.send_replace(true);
    let completion = timeout(Duration::from_millis(750), read_sse_message(&mut response))
        .await
        .context("SSE stream did not complete promptly during shutdown")??;
    assert_eq!(
        completion
            .pointer("/result/resultType")
            .and_then(Value::as_str),
        Some("complete")
    );
    assert_eq!(
        completion.get("id").and_then(Value::as_str),
        Some("listen-close")
    );
    assert_eq!(
        completion
            .pointer("/result/_meta/io.modelcontextprotocol~1subscriptionId")
            .and_then(Value::as_str),
        Some("listen-close")
    );
    assert_eq!(
        completion
            .pointer("/result/_meta/io.modelcontextprotocol~1serverInfo/name")
            .and_then(Value::as_str),
        Some("mcp-pool")
    );
    assert!(
        timeout(Duration::from_millis(750), response.chunk())
            .await
            .context("SSE response did not close after completion")??
            .is_none()
    );
    timeout(Duration::from_millis(750), server)
        .await
        .context("HTTP server did not finish graceful shutdown")???;
    fixture.wait_for(2, 1, 0).await?;
    Ok(())
}

#[tokio::test]
async fn malformed_modern_listener_filter_returns_json_rpc_error() -> Result<()> {
    let state = super::state().await?;
    let (url, shutdown, server) = start_http(state).await?;
    let response = reqwest::Client::new()
        .post(url.as_str())
        .header("accept", ACCEPT)
        .header("mcp-protocol-version", MODERN_VERSION)
        .header("mcp-method", "subscriptions/listen")
        .json(&json!({
            "jsonrpc":"2.0",
            "id":"bad-filter",
            "method":"subscriptions/listen",
            "params":{"_meta":modern_metadata(),"notifications":"toolsListChanged"}
        }))
        .send()
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.json::<Value>().await?;
    assert_eq!(body.get("id").and_then(Value::as_str), Some("bad-filter"));
    assert_eq!(
        body.pointer("/error/code").and_then(Value::as_i64),
        Some(-32602)
    );
    shutdown.send_replace(true);
    server.await??;
    Ok(())
}

async fn notification_state() -> Result<(BridgeState, Fixture)> {
    let fixture = Fixture::new(
        "alpha",
        vec![json!({
            "name":"ping",
            "description":"fixture ping",
            "inputSchema":{"type":"object","properties":{"value":{"type":"string"}}}
        })],
    )
    .with_client_deadline(Duration::from_millis(200));
    let bridge = test_fixture::default_state(vec![fixture.clone()]).await?;
    Ok((bridge, fixture))
}

async fn start_http(
    bridge: Arc<BridgeState>,
) -> Result<(
    reqwest::Url,
    tokio::sync::watch::Sender<bool>,
    JoinHandle<std::io::Result<()>>,
)> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let address = listener.local_addr()?;
    let http_state = http_state_at(bridge, address.port());
    let shutdown = http_state.shutdown.clone();
    let mut shutdown_receiver = shutdown.subscribe();
    let application = Router::new()
        .fallback(handle_request)
        .with_state(http_state);
    let server = tokio::spawn(async move {
        axum::serve(listener, application)
            .with_graceful_shutdown(async move {
                if !*shutdown_receiver.borrow() && shutdown_receiver.changed().await.is_err() {}
            })
            .await
    });
    Ok((
        reqwest::Url::parse(&format!("http://{address}/mcp"))?,
        shutdown,
        server,
    ))
}

fn modern_metadata() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion":MODERN_VERSION,
        "io.modelcontextprotocol/clientInfo":{"name":"fixture","version":"1"},
        "io.modelcontextprotocol/clientCapabilities":{}
    })
}

fn listen_request(id: &str) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "method":"subscriptions/listen",
        "params":{
            "_meta":modern_metadata(),
            "notifications":{"toolsListChanged":true}
        }
    })
}

async fn read_sse_message(response: &mut reqwest::Response) -> Result<Value> {
    let mut buffer = String::new();
    loop {
        let chunk = timeout(Duration::from_secs(2), response.chunk())
            .await
            .context("SSE message deadline exceeded")??
            .context("SSE stream ended before its next message")?;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(frame_end) = buffer.find("\n\n") {
            let frame = buffer.drain(..frame_end + 2).collect::<String>();
            let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) else {
                continue;
            };
            return serde_json::from_str(data).context("SSE frame had invalid JSON");
        }
    }
}
