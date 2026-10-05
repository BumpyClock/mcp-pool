use super::*;

/// Sampling and roots callbacks require the capability advertised at initialize.
/// Other callbacks prefer the last active client. Preserve the server's id.
pub(super) async fn route_server_request(
    line: &str,
    value: &Value,
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    last_active_client: &Arc<Mutex<Option<String>>>,
    client_capabilities: &Arc<Mutex<HashMap<String, ClientCapabilities>>>,
    request_tx: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
) {
    let method = value.get("method").and_then(Value::as_str).unwrap_or("?");
    if let Some(required) = required_capability(method) {
        let target = capable_client(clients, client_capabilities, last_active_client, required);
        if let Some(client_id) = target {
            diagnostics::log(format!(
                "pool_server_request_routed client_id={} method={}",
                client_id, method
            ));
            send_to_client(&client_id, line.to_string(), clients).await;
            return;
        }

        let Some(server_id) = non_null_id(value).cloned() else {
            diagnostics::log(format!(
                "pool_server_request_dropped method={} reason=no_capable_client",
                method
            ));
            return;
        };
        let response = build_error_response(
            server_id,
            -32001,
            &format!("no capable downstream client connected for {method}"),
        );
        send_to_upstream(response, request_tx, method).await;
        return;
    }

    diagnostics::log(format!("pool_server_request_fallback method={}", method));
    // Snapshot last-active (releasing its lock) before locking clients, so the
    // two parking_lot mutexes are never held nested.
    let last_active = last_active_client.lock().clone();
    let target = {
        let clients_guard = clients.lock();
        match last_active {
            Some(id) if clients_guard.contains_key(&id) => Some(id),
            // Fall back to any one connected client (first by iteration order).
            _ => clients_guard.keys().next().cloned(),
        }
    };

    let Some(client_id) = target else {
        diagnostics::log(format!(
            "pool_server_request_dropped bytes={} reason=no_clients",
            line.len()
        ));
        return;
    };
    diagnostics::log(format!(
        "pool_server_request_routed client_id={}",
        client_id
    ));
    send_to_client(&client_id, line.to_string(), clients).await;
}

#[derive(Debug, Clone, Copy)]
enum RequiredCapability {
    Sampling,
    Roots,
}

fn required_capability(method: &str) -> Option<RequiredCapability> {
    if method.starts_with("sampling/") {
        Some(RequiredCapability::Sampling)
    } else if method.starts_with("roots/") {
        Some(RequiredCapability::Roots)
    } else {
        None
    }
}

fn capable_client(
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    client_capabilities: &Arc<Mutex<HashMap<String, ClientCapabilities>>>,
    last_active_client: &Arc<Mutex<Option<String>>>,
    required: RequiredCapability,
) -> Option<String> {
    let last_active = last_active_client.lock().clone();
    let clients_guard = clients.lock();
    let capabilities_guard = client_capabilities.lock();
    if let Some(client_id) = last_active
        && clients_guard.contains_key(&client_id)
        && capabilities_guard
            .get(&client_id)
            .is_some_and(|capabilities| has_required_capability(capabilities, required))
    {
        return Some(client_id);
    }
    capabilities_guard
        .iter()
        .find_map(|(client_id, capabilities)| {
            if clients_guard.contains_key(client_id)
                && has_required_capability(capabilities, required)
            {
                Some(client_id.clone())
            } else {
                None
            }
        })
}

fn has_required_capability(
    capabilities: &ClientCapabilities,
    required: RequiredCapability,
) -> bool {
    match required {
        RequiredCapability::Sampling => capabilities.sampling,
        RequiredCapability::Roots => capabilities.roots,
    }
}

async fn send_to_upstream(
    payload: String,
    request_tx: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    method: &str,
) {
    let sender = request_tx.lock().clone();
    let Some(sender) = sender else {
        diagnostics::log(format!(
            "pool_server_request_error_send_failed method={} reason=upstream_unavailable",
            method
        ));
        return;
    };
    if sender.send(payload).await.is_err() {
        diagnostics::log(format!(
            "pool_server_request_error_send_failed method={} reason=upstream_closed",
            method
        ));
    }
}

/// Deliver one already-serialized payload to a single client. Preserves the
/// head-of-line-blocking-aware try_send-then-send pattern: the response router is
/// a single task shared by every client, so when a client drains its bounded
/// channel slower than messages arrive, `send().await` parks the *whole* router.
/// try_send first records (and times) the stall instead of stalling silently. A
/// payload for a vanished client is dropped, never rebroadcast.
pub(super) async fn send_to_client(
    client_id: &str,
    payload: String,
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
) {
    let sender = clients.lock().get(client_id).cloned();
    let Some(sender) = sender else {
        diagnostics::log(format!("pool_response_orphaned client_id={}", client_id));
        return;
    };

    let byte_len = payload.len();
    match sender.try_send(payload) {
        Ok(()) => diagnostics::log(format!(
            "pool_response_routed client_id={} bytes={}",
            client_id, byte_len
        )),
        Err(mpsc::error::TrySendError::Full(payload)) => {
            let blocked_since = Instant::now();
            diagnostics::log(format!(
                "pool_router_blocked client_id={} reason=client_channel_full",
                client_id
            ));
            if sender.send(payload).await.is_ok() {
                diagnostics::log(format!(
                    "pool_response_routed client_id={} bytes={} blocked_ms={}",
                    client_id,
                    byte_len,
                    blocked_since.elapsed().as_millis()
                ));
            } else {
                diagnostics::log(format!("pool_response_send_failed client_id={}", client_id));
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            diagnostics::log(format!("pool_response_send_failed client_id={}", client_id));
        }
    }
}
pub(super) async fn broadcast_to_all(
    line: &str,
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
) {
    let senders: Vec<ClientSender> = clients.lock().values().cloned().collect();
    for sender in &senders {
        if sender.send(line.to_string()).await.is_err() {
            diagnostics::log("pool_broadcast_client_closed");
        }
    }
    diagnostics::log(format!(
        "pool_response_broadcast bytes={} clients={}",
        line.len(),
        senders.len()
    ));
}

/// True if the message carries a `method` field. JSON-RPC requests and
/// notifications have `method`; responses (carrying `result`/`error`) do not, so
/// this is the primary discriminator between the two families.
pub(super) fn message_has_method(value: &Value) -> bool {
    value.get("method").is_some()
}

/// The message's `id` if present and non-null. A null/absent id marks a
/// notification; a non-null id marks a request (with `method`) or a response
/// (without `method`).
pub(super) fn non_null_id(value: &Value) -> Option<&Value> {
    match value.get("id") {
        Some(id) if !id.is_null() => Some(id),
        _ => None,
    }
}
