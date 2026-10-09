use std::convert::Infallible;
use std::time::Duration;

use axum::body::Body;
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue};
use axum::http::{Response, StatusCode};
use serde_json::Value;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{OwnedSemaphorePermit, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, interval, sleep};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::super::BridgeClient;
use super::{HttpState, rpc};

const MAX_QUEUED_EVENTS: usize = 4;
const MAX_SSE_MESSAGE_BYTES: usize = 1024 * 1024;
#[cfg(not(test))]
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
#[cfg(test)]
const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(25);
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(100);
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(2);

pub(super) async fn start(
    state: HttpState,
    subscription: rpc::ListenSubscription,
    only_server: Option<&str>,
    request_permit: OwnedSemaphorePermit,
) -> Response<Body> {
    let listener_servers = if subscription.tools_list_changed {
        match state.bridge.notification_servers(only_server) {
            Ok(servers) => servers,
            Err(_) => return json_failure(subscription.id),
        }
    } else {
        Vec::new()
    };
    let mut listener_permits = Vec::with_capacity(listener_servers.len());
    for _ in &listener_servers {
        match state.notification_listeners.clone().try_acquire_owned() {
            Ok(permit) => listener_permits.push(permit),
            Err(_) => return service_unavailable(),
        }
    }

    let mut pending = JoinSet::new();
    for server in listener_servers {
        let bridge = state.bridge.clone();
        pending.spawn(async move {
            let result = bridge.connect_notification_client(&server).await;
            (server, result)
        });
    }
    let mut listeners = Vec::with_capacity(pending.len());
    while let Some(completed) = pending.join_next().await {
        match completed {
            Ok((server, Ok(client))) => listeners.push((server, client)),
            Ok((_, Err(_))) | Err(_) => {
                pending.abort_all();
                return json_failure(subscription.id);
            }
        }
    }

    let (sender, receiver) = mpsc::channel(MAX_QUEUED_EVENTS);
    let shutdown = state.shutdown.subscribe();
    let bridge = state.bridge.clone();
    let subscription_id = subscription.id;
    let listen_for_tools = subscription.tools_list_changed;
    tokio::spawn(run_subscription(
        sender,
        bridge,
        listeners,
        subscription_id,
        listen_for_tools,
        shutdown,
        request_permit,
        listener_permits,
    ));

    let event_stream =
        ReceiverStream::new(receiver).map(|event| Ok::<_, Infallible>(event.into_bytes()));
    let mut response = Response::new(Body::from_stream(event_stream));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response.headers_mut().insert(
        CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    response
        .headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    response
}

#[allow(clippy::too_many_arguments)]
async fn run_subscription(
    sender: mpsc::Sender<String>,
    bridge: std::sync::Arc<super::BridgeState>,
    listener_clients: Vec<(String, Box<dyn BridgeClient>)>,
    subscription_id: Value,
    listen_for_tools: bool,
    mut shutdown: watch::Receiver<bool>,
    _request_permit: OwnedSemaphorePermit,
    _listener_permits: Vec<OwnedSemaphorePermit>,
) {
    let acknowledgement = rpc::subscription_acknowledgement(&subscription_id, listen_for_tools);
    let Some(acknowledgement) = sse_message(&acknowledgement) else {
        return;
    };
    if sender.send(acknowledgement).await.is_err() {
        return;
    }
    if *shutdown.borrow() {
        if let Some(completion) = sse_message(&rpc::subscription_completion(&subscription_id))
            && sender.send(completion).await.is_err()
        {
            return;
        }
        return;
    }

    let mut listener_tasks = JoinSet::new();
    for (server, client) in listener_clients {
        let bridge = std::sync::Arc::clone(&bridge);
        let sender = sender.clone();
        let subscription_id = subscription_id.clone();
        listener_tasks.spawn(async move {
            forward_notifications(bridge, server, client, sender, subscription_id).await
        });
    }

    let mut keepalive = interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = sender.closed() => break,
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    let completion = rpc::subscription_completion(&subscription_id);
                    if let Some(completion) = sse_message(&completion)
                        && sender.send(completion).await.is_err() {
                            break;
                        }
                    break;
                }
            }
            _ = keepalive.tick() => {
                if sender.capacity() > 1 {
                    match sender.try_send(": keepalive\n\n".to_owned()) {
                        Ok(()) | Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Closed(_)) => break,
                    }
                }
            }
            completed = listener_tasks.join_next(), if !listener_tasks.is_empty() => {
                match completed {
                    Some(Ok(Ok(()))) | None => {}
                    Some(Ok(Err(()))) | Some(Err(_)) => {
                        eprintln!("mcp-pool bridge notification listener task failed.");
                        break;
                    }
                }
            }
        }
    }

    listener_tasks.abort_all();
    while listener_tasks.join_next().await.is_some() {}
}

/// A queued invalidation subsumes later tool-list invalidations.
async fn forward_notifications(
    bridge: std::sync::Arc<super::BridgeState>,
    server: String,
    mut client: Box<dyn BridgeClient>,
    sender: mpsc::Sender<String>,
    subscription_id: Value,
) -> std::result::Result<(), ()> {
    let mut reconnect_delay = INITIAL_RECONNECT_DELAY;
    loop {
        let received = tokio::select! {
            _ = sender.closed() => return Ok(()),
            result = client.wait_for_notification() => result,
        };
        match received {
            Ok(Some(mut message)) => {
                reconnect_delay = INITIAL_RECONNECT_DELAY;
                if rpc::is_tools_list_changed(&message)
                    && rpc::stamp_subscription_id(&mut message, &subscription_id)
                {
                    if let Some(frame) = sse_message(&message) {
                        match sender.try_send(frame) {
                            Ok(()) => {}
                            Err(TrySendError::Full(_)) => {}
                            Err(TrySendError::Closed(_)) => return Ok(()),
                        }
                    } else {
                        eprintln!(
                            "mcp-pool bridge notification from '{server}' exceeded the SSE message limit."
                        );
                        return Err(());
                    }
                }
            }
            Ok(None) | Err(_) => {
                eprintln!("mcp-pool bridge notification listener for '{server}' disconnected.");
                loop {
                    tokio::select! {
                        _ = sender.closed() => return Ok(()),
                        _ = sleep(reconnect_delay) => {}
                    }
                    match bridge.connect_notification_client(&server).await {
                        Ok(reconnected) => {
                            client = reconnected;
                            reconnect_delay = INITIAL_RECONNECT_DELAY;
                            break;
                        }
                        Err(_) => {
                            reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
                        }
                    }
                }
            }
        }
    }
}

fn sse_message(message: &Value) -> Option<String> {
    let encoded = serde_json::to_string(message).ok()?;
    if encoded.len() > MAX_SSE_MESSAGE_BYTES {
        return None;
    }
    Some(format!("event: message\ndata: {encoded}\n\n"))
}

fn json_failure(id: Value) -> Response<Body> {
    let mut response = Response::new(Body::from(rpc::server_failure_response(id).to_string()));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

fn service_unavailable() -> Response<Body> {
    let mut response = Response::new(Body::from("Too many active MCP notification listeners"));
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}
