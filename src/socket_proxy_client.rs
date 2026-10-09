use super::*;

type PendingForward = (
    Option<u64>,
    std::pin::Pin<Box<dyn std::future::Future<Output = io::Result<()>> + Send>>,
);

/// Pump one client connection: read newline-delimited JSON-RPC requests from the
/// client, translate each request id to a pool-unique id, forward to the
/// upstream, and write routed responses back as they arrive on `rx`. The
/// (client_id, original_id) mapping is recorded under the pool id so responses
/// route back to the right client with the id that client expects.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_client(
    stream: LocalStream,
    client_id: String,
    request_tx: Arc<Mutex<Option<mpsc::Sender<crate::upstream::UpstreamRequest>>>>,
    upstream_ready: Arc<Notify>,
    id_allocator: Arc<IdAllocator>,
    request_map: RequestMap,
    handshake_cache: HandshakeCacheRef,
    client_capabilities: Arc<Mutex<HashMap<String, ClientCapabilities>>>,
    last_active_client: Arc<Mutex<Option<String>>>,
    clients: Arc<Mutex<HashMap<String, ClientSender>>>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    expiration_changed: Arc<Notify>,
    remote: bool,
    shared_timeout: Duration,
    mut rx: mpsc::Receiver<String>,
) {
    diagnostics::log(format!(
        "pool_handle_client_started client_id={}",
        client_id
    ));

    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut buffer = String::new();
    let mut parse_failures = 0u32;
    let mut pending_forward: Option<PendingForward> = None;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            result = async {
                match pending_forward.as_mut() {
                    Some((_, forwarding)) => forwarding.await,
                    None => std::future::pending().await,
                }
            } => {
                pending_forward = None;
                if let Err(error) = result {
                    diagnostics::log(format!(
                        "pool_request_forward_failed client_id={client_id} error={error}"
                    ));
                    break;
                }
            },
            read_result = reader.read_line(&mut buffer), if pending_forward.is_none() => match read_result {
                Ok(0) => {
                    diagnostics::log(format!("pool_client_disconnected client_id={}", client_id));
                    break;
                }
                Ok(_) => {
                    let line = buffer.trim_end_matches('\n').to_string();
                    buffer.clear();
                    if line.is_empty() {
                        continue;
                    }
                    // Classify by JSON-RPC message kind (presence of `method`
                    // and a non-null `id`) rather than by id alone. Only a
                    // REQUEST gets its id rewritten and stored; a client
                    // RESPONSE (no `method`) answers a server-initiated request
                    // and carries the SERVER's id, so it must pass through with
                    // its id intact and unstored. Notifications and unparseable
                    // lines forward verbatim. A cacheable handshake REQUEST whose
                    // success response is already cached is answered directly,
                    // skipping the upstream entirely.
                    let action = match serde_json::from_str::<Value>(&line) {
                        Ok(mut value) if value.is_object() => 'message: {
                            let timeout_ms = match crate::request_deadline::timeout_ms(&value) {
                                Ok(timeout_ms) => timeout_ms,
                                Err(error) => break 'message ClientAction::Cached(build_error_response(
                                    non_null_id(&value).cloned().unwrap_or(Value::Null),
                                    -32602,
                                    &error,
                                )),
                            };
                            let cache_key = cacheable_request(&value);
                            let floor = Duration::from_secs(REQUEST_TTL_SECS).max(
                                if cache_key.is_some() { shared_timeout } else { Duration::ZERO }
                            );
                            let expires_after = timeout_ms
                                .map(Duration::from_millis)
                                .unwrap_or(floor)
                                .max(floor);
                            if Instant::now().checked_add(expires_after).is_none() {
                                break 'message ClientAction::Cached(build_error_response(
                                    non_null_id(&value).cloned().unwrap_or(Value::Null),
                                    -32602,
                                    "Pool request timeout exceeds the clock range",
                                ));
                            }
                            let transport_timeout = timeout_ms.map(Duration::from_millis)
                                .unwrap_or(shared_timeout).max(shared_timeout);
                            let Some(transport_deadline) = Instant::now().checked_add(transport_timeout) else {
                                break 'message ClientAction::Cached(build_error_response(
                                    non_null_id(&value).cloned().unwrap_or(Value::Null),
                                    -32602, "Pool request timeout exceeds the clock range",
                                ));
                            };
                            let line = if !remote && timeout_ms.is_some() {
                                if let Some(object) = value.as_object_mut() {
                                    object.remove(crate::request_deadline::TIMEOUT_FIELD);
                                }
                                value.to_string()
                            } else {
                                line
                            };
                            // Clone the original id (ending the borrow) before
                            // moving the object into `with_id`.
                            let original_id = non_null_id(&value).cloned();
                            match (message_has_method(&value), original_id) {
                                // REQUEST: method + non-null id.
                                (true, Some(original_id)) => {
                                    let method = value
                                        .get("method")
                                        .and_then(Value::as_str)
                                        .map(str::to_string);
                                    // Tool name only for tools/call, for log
                                    // enrichment; never the call arguments.
                                    let tool = if method.as_deref() == Some("tools/call") {
                                        tool_name(&value)
                                    } else {
                                        None
                                    };
                                    diagnostics::log(format!(
                                        "pool_request_received client_id={} method={}{} has_id=true bytes={}",
                                        client_id,
                                        method.as_deref().unwrap_or("?"),
                                        tool.as_deref()
                                            .map(|t| format!(" tool={t}"))
                                            .unwrap_or_default(),
                                        line.len()
                                    ));
                                    if method.as_deref() == Some("initialize") {
                                        client_capabilities.lock().insert(
                                            client_id.clone(),
                                            parse_client_capabilities(&value),
                                        );
                                    }
                                    let (discovery, coalesced_event) = match cache_key {
                                        Some(CacheableMethod::Initialize) => (
                                            prepare_initialize_request(
                                                &handshake_cache,
                                                &client_id,
                                                original_id.clone(),
                                                expires_after,
                                                transport_deadline,
                                            ),
                                            "pool_initialize_coalesced",
                                        ),
                                        Some(CacheableMethod::ToolsList) => (
                                            prepare_tools_list_request(
                                                &handshake_cache,
                                                &client_id,
                                                original_id.clone(),
                                                expires_after,
                                                transport_deadline,
                                            ),
                                            "pool_tools_list_coalesced",
                                        ),
                                        None => (DiscoveryAction::Leader, ""),
                                    };
                                    match discovery {
                                        DiscoveryAction::Cached(response) => {
                                            diagnostics::log(format!(
                                                "pool_cache_hit method={} client_id={}",
                                                method.as_deref().unwrap_or("?"), client_id
                                            ));
                                            ClientAction::Cached(response)
                                        }
                                        DiscoveryAction::Coalesced => {
                                            diagnostics::log(format!(
                                                "{coalesced_event} client_id={client_id}"
                                            ));
                                            ClientAction::Drop
                                        }
                                        DiscoveryAction::Leader => {
                                            if cache_key.is_some() {
                                                diagnostics::log(format!(
                                                    "pool_cache_miss method={} client_id={}",
                                                    method.as_deref().unwrap_or("?"), client_id
                                                ));
                                            }
                                            let pool_id = id_allocator.allocate();
                                            // Store the real method so the response
                                            // route log reports the actual method
                                            // (e.g. tools/call) rather than `?`.
                                            // Key the pending request through the same
                                            // canonical helper route_response uses to
                                            // look it up, so the insert and lookup keys
                                            // cannot drift (numeric id -> identical
                                            // string).
                                            request_map.lock().insert(
                                                jsonrpc::id_key(&Value::from(pool_id)),
                                                PendingRequestInfo {
                                                    client_id: client_id.clone(),
                                                    original_id,
                                                    method: method.clone(),
                                                    cache_key,
                                                    tool: tool.clone(),
                                                    inserted_at: Instant::now(),
                                                    expires_after,
                                                },
                                            );
                                            // Record this client as most-recently-active
                                            // so a server-initiated callback can route
                                            // back to it (see route_server_request).
                                            *last_active_client.lock() = Some(client_id.clone());
                                            match value {
                                                Value::Object(object) => ClientAction::Forward {
                                                    line: jsonrpc::with_id(object, Value::from(pool_id)),
                                                    method,
                                                    tool,
                                                    pool_id: Some(pool_id),
                                                },
                                                // Unreachable: guarded by is_object
                                                // above, but match instead of unwrap to
                                                // stay panic-free.
                                                _ => ClientAction::Forward {
                                                    line,
                                                    method,
                                                    tool,
                                                    pool_id: Some(pool_id),
                                                },
                                            }
                                        }
                                    }
                                }
                                // NOTIFICATION (method, no id) or RESPONSE
                                // (no method): forward verbatim, never store.
                                _ => {
                                    if handshake_cache.lock().swallow_initialized(&value) {
                                        diagnostics::log(format!(
                                            "pool_cached_initialized_swallowed client_id={}",
                                            client_id
                                        ));
                                        ClientAction::Drop
                                    } else {
                                        let method = value
                                            .get("method")
                                            .and_then(Value::as_str)
                                            .map(str::to_string);
                                        diagnostics::log(format!(
                                            "pool_request_received client_id={} method={} has_id=false bytes={}",
                                            client_id,
                                            method.as_deref().unwrap_or("?"),
                                            line.len()
                                        ));
                                        ClientAction::Forward {
                                            line,
                                            method,
                                            tool: None,
                                            pool_id: None,
                                        }
                                    }
                                }
                            }
                        }
                        Ok(_) => ClientAction::Forward {
                            line,
                            method: None,
                            tool: None,
                            pool_id: None,
                        },
                        Err(_) => {
                            if parse_failures < 3 {
                                // Throttle log spam from a chatty malformed sender.
                                parse_failures += 1;
                                diagnostics::log(format!(
                                    "pool_request_parse_failed client_id={} bytes={}",
                                    client_id, line.len()
                                ));
                            }
                            ClientAction::Forward {
                                line,
                                method: None,
                                tool: None,
                                pool_id: None,
                            }
                        }
                    };

                    expiration_changed.notify_one();
                    let (forward_line, forward_method, forward_tool, forward_pool_id) = match action {
                        // Cache hit: reply directly to this client. handle_client
                        // owns write_half and the select! arms never run
                        // concurrently, so writing here is not re-entrant.
                        ClientAction::Cached(response) => {
                            let mut bytes = response.into_bytes();
                            bytes.push(b'\n');
                            if let Err(err) = write_half.write_all(&bytes).await {
                                diagnostics::log(format!(
                                    "pool_client_write_failed client_id={} error={}",
                                    client_id, err
                                ));
                                break;
                            }
                            if let Err(err) = write_half.flush().await {
                                diagnostics::log(format!(
                                    "pool_client_flush_failed client_id={} error={}",
                                    client_id, err
                                ));
                                break;
                            }
                            continue;
                        }
                        ClientAction::Drop => continue,
                        ClientAction::Forward {
                            line,
                            method,
                            tool,
                            pool_id,
                        } => (line, method, tool, pool_id),
                    };

                    let request_tx = request_tx.clone();
                    let forward_deadline = forward_pool_id.and_then(|pool_id| {
                        request_map.lock().get(&jsonrpc::id_key(&Value::from(pool_id)))
                            .and_then(|pending| pending.cache_key)
                    }).and_then(|method| handshake_cache.lock().deadline(method));
                    let upstream_ready = upstream_ready.clone();
                    let shutdown = shutdown.clone();
                    let request_map = request_map.clone();
                    let client_id = client_id.clone();
                    // One deferred send preserves input order without blocking routed responses.
                    pending_forward = Some((forward_pool_id, Box::pin(async move {
                        let Some(sender) = acquire_request_sender(
                            &request_tx, &upstream_ready, &shutdown, &client_id,
                        ).await else { return Ok(()) };
                        let permit = sender.reserve_owned().await.map_err(io::Error::other)?;
                        let pending = request_map.lock();
                        if shutdown.load(Ordering::SeqCst)
                            || forward_pool_id.is_some_and(|pool_id| {
                                !pending.contains_key(&jsonrpc::id_key(&Value::from(pool_id)))
                            })
                        {
                            return Ok(());
                        }
                        let bytes = forward_line.len();
                        // Expiration and dispatch are atomic with respect to the request map.
                        permit.send(crate::upstream::UpstreamRequest {
                            line: forward_line,
                            deadline: forward_deadline,
                        });
                        drop(pending);
                        diagnostics::log(format!(
                            "pool_request_forwarded client_id={} method={}{} pool_id={} bytes={}",
                            client_id,
                            forward_method.as_deref().unwrap_or("?"),
                            forward_tool
                                .as_deref()
                                .map(|t| format!(" tool={t}"))
                                .unwrap_or_default(),
                            forward_pool_id
                                .map(|pool_id| pool_id.to_string())
                                .unwrap_or_else(|| "?".to_string()),
                            bytes
                        ));
                        Ok(())
                    })));
                }
                Err(err) => {
                    diagnostics::log(format!(
                        "pool_client_read_error client_id={} error={}",
                        client_id, err
                    ));
                    break;
                }
            },
            message = rx.recv() => match message {
                Some(message) => {
                    let mut bytes = message.into_bytes();
                    bytes.push(b'\n');
                    if let Err(err) = write_half.write_all(&bytes).await {
                        diagnostics::log(format!(
                            "pool_client_write_failed client_id={} error={}",
                            client_id, err
                        ));
                        break;
                    }
                    if let Err(err) = write_half.flush().await {
                        diagnostics::log(format!(
                            "pool_client_flush_failed client_id={} error={}",
                            client_id, err
                        ));
                        break;
                    }
                    if pending_forward.as_ref().is_some_and(|(pool_id, _)| {
                        pool_id.is_some_and(|pool_id| {
                            !request_map.lock().contains_key(&jsonrpc::id_key(&Value::from(pool_id)))
                        })
                    }) {
                        pending_forward = None;
                    }
                }
                None => break,
            },
            _ = shutdown_notify.notified() => break,
        }
    }

    if shutdown.load(Ordering::SeqCst) {
        while let Ok(message) = rx.try_recv() {
            let mut bytes = message.into_bytes();
            bytes.push(b'\n');
            if let Err(error) = write_half.write_all(&bytes).await {
                diagnostics::log(format!(
                    "pool_client_drain_failed client_id={client_id} error={error}"
                ));
                break;
            }
        }
        if let Err(error) = write_half.flush().await {
            diagnostics::log(format!(
                "pool_client_drain_failed client_id={client_id} error={error}"
            ));
        }
    }

    // Drop this client's in-flight requests so responses are not routed to a
    // (now closed) sender, then remove it from the client table.
    request_map
        .lock()
        .retain(|_, pending| pending.client_id != client_id || pending.cache_key.is_some());
    {
        let mut cache = handshake_cache.lock();
        if let Initialization::InFlight { waiters, .. } = &mut cache.initialize {
            waiters.retain(|waiter| waiter.client_id != client_id);
        }
        cache
            .tools_list
            .waiters
            .retain(|waiter| waiter.client_id != client_id);
    }
    clients.lock().remove(&client_id);
    client_capabilities.lock().remove(&client_id);
    expiration_changed.notify_one();
}
