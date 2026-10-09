use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, mpsc};
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

use super::{handle_request, http_state_at};
use crate::mcp_bridge::{BridgeClient, BridgeState, ClientFuture, ReconnectFactory};

const MODERN_VERSION: &str = "2026-07-28";
const ACCEPT: &str = "application/json, text/event-stream";

struct NotificationFixtureClient {
    receiver: Arc<Mutex<mpsc::UnboundedReceiver<Value>>>,
    active_listeners: Arc<AtomicUsize>,
    tracked: bool,
}

impl NotificationFixtureClient {
    fn regular(
        receiver: Arc<Mutex<mpsc::UnboundedReceiver<Value>>>,
        active_listeners: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            receiver,
            active_listeners,
            tracked: false,
        }
    }

    fn listener(
        receiver: Arc<Mutex<mpsc::UnboundedReceiver<Value>>>,
        active_listeners: Arc<AtomicUsize>,
    ) -> Self {
        active_listeners.fetch_add(1, Ordering::SeqCst);
        Self {
            receiver,
            active_listeners,
            tracked: true,
        }
    }
}

impl Drop for NotificationFixtureClient {
    fn drop(&mut self) {
        if self.tracked {
            self.active_listeners.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

impl BridgeClient for NotificationFixtureClient {
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
        Box::pin(
            async move { Ok(json!({"content":[{"type":"text","text":"fixture"}],"echo":params})) },
        )
    }

    fn wait_for_notification(&mut self) -> ClientFuture<'_, Option<Value>> {
        let receiver = Arc::clone(&self.receiver);
        Box::pin(async move { Ok(receiver.lock().await.recv().await) })
    }
}

#[tokio::test]
async fn modern_http_negotiation_and_requests_follow_discovery_envelope() -> Result<()> {
    let state = super::state()?;
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
    let state = super::state()?;
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
    let (state, notification_sender, active_listeners, listener_connections) =
        notification_state()?;
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
    sleep(Duration::from_millis(150)).await;
    assert_eq!(listener_connections.load(Ordering::SeqCst), 1);
    notification_sender
        .send(json!({
            "jsonrpc":"2.0",
            "method":"notifications/resources/list_changed",
            "params":{}
        }))
        .map_err(|_| anyhow::anyhow!("Notification fixture stopped early."))?;
    notification_sender
        .send(json!({
            "jsonrpc":"2.0",
            "method":"notifications/tools/list_changed",
            "params":{}
        }))
        .map_err(|_| anyhow::anyhow!("Notification fixture stopped early."))?;
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

    drop(response);
    wait_for_listener_count(active_listeners, 0).await;
    shutdown.send_replace(true);
    server.await??;
    Ok(())
}

#[tokio::test]
async fn modern_listener_completes_before_graceful_http_shutdown() -> Result<()> {
    let (state, _, active_listeners, _) = notification_state()?;
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
    assert_eq!(active_listeners.load(Ordering::SeqCst), 1);

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
    assert_eq!(active_listeners.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test]
async fn malformed_modern_listener_filter_returns_json_rpc_error() -> Result<()> {
    let state = super::state()?;
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

type NotificationFixture = (
    BridgeState,
    mpsc::UnboundedSender<Value>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
);

fn notification_state() -> Result<NotificationFixture> {
    let (notification_sender, notification_receiver) = mpsc::unbounded_channel();
    let notification_receiver = Arc::new(Mutex::new(notification_receiver));
    let active_listeners = Arc::new(AtomicUsize::new(0));
    let listener_connections = Arc::new(AtomicUsize::new(0));
    let regular = NotificationFixtureClient::regular(
        Arc::clone(&notification_receiver),
        Arc::clone(&active_listeners),
    );
    let receiver_for_reconnect = Arc::clone(&notification_receiver);
    let active_for_reconnect = Arc::clone(&active_listeners);
    let connections_for_reconnect = Arc::clone(&listener_connections);
    let reconnect: ReconnectFactory = Arc::new(move || {
        let receiver = Arc::clone(&receiver_for_reconnect);
        let active = Arc::clone(&active_for_reconnect);
        let connections = Arc::clone(&connections_for_reconnect);
        Box::pin(async move {
            connections.fetch_add(1, Ordering::SeqCst);
            Ok(
                Box::new(NotificationFixtureClient::listener(receiver, active))
                    as Box<dyn BridgeClient>,
            )
        }) as ClientFuture<'static, Box<dyn BridgeClient>>
    });
    let bridge = BridgeState::with_reconnectable_clients(vec![(
        "alpha".to_owned(),
        Box::new(regular),
        reconnect,
        Duration::from_millis(20),
    )])?;
    Ok((
        bridge,
        notification_sender,
        active_listeners,
        listener_connections,
    ))
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

async fn wait_for_listener_count(active: Arc<AtomicUsize>, expected: usize) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while active.load(Ordering::SeqCst) != expected && tokio::time::Instant::now() < deadline {
        sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(active.load(Ordering::SeqCst), expected);
}
