use super::*;

/// Pump one client connection: read newline-delimited JSON-RPC requests from the
/// client, translate each request id to a pool-unique id, forward to the
/// upstream, and write routed responses back as they arrive on `rx`. The
/// (client_id, original_id) mapping is recorded under the pool id so responses
/// route back to the right client with the id that client expects.
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_client(
    stream: LocalStream,
    client_id: String,
    request_tx: Arc<Mutex<Option<mpsc::Sender<String>>>>,
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

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            read_result = reader.read_line(&mut buffer) => match read_result {
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
                        Ok(value) if value.is_object() => {
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
                                    let cache_key = cacheable_request(&value);
                                    match cache_key {
                                        Some(CacheableMethod::Initialize) => {
                                            match prepare_initialize_request(
                                                &handshake_cache,
                                                &client_id,
                                                original_id.clone(),
                                            ) {
                                            DiscoveryAction::Cached(response) => {
                                                diagnostics::log(format!(
                                                    "pool_cache_hit method=initialize client_id={}",
                                                    client_id
                                                ));
                                                ClientAction::Cached(response)
                                            }
                                            DiscoveryAction::Coalesced => {
                                                diagnostics::log(format!(
                                                    "pool_initialize_coalesced client_id={}", client_id
                                                ));
                                                ClientAction::Drop
                                            }
                                            DiscoveryAction::Leader => {
                                                diagnostics::log(format!(
                                                    "pool_cache_miss method=initialize client_id={}",
                                                    client_id
                                                ));
                                                let pool_id = id_allocator.allocate();
                                                request_map.lock().insert(
                                                    jsonrpc::id_key(&Value::from(pool_id)),
                                                    PendingRequestInfo {
                                                        client_id: client_id.clone(),
                                                        original_id,
                                                        method: Some("initialize".to_string()),
                                                        cache_key,
                                                        tool: None,
                                                        inserted_at: Instant::now(),
                                                    },
                                                );
                                                *last_active_client.lock() = Some(client_id.clone());
                                                match value.clone() {
                                                    Value::Object(object) => ClientAction::Forward {
                                                        line: jsonrpc::with_id(object, Value::from(pool_id)),
                                                        method: Some("initialize".to_string()),
                                                        tool: None,
                                                        pool_id: Some(pool_id),
                                                    },
                                                    _ => ClientAction::Forward {
                                                        line: line.clone(),
                                                        method: Some("initialize".to_string()),
                                                        tool: None,
                                                        pool_id: Some(pool_id),
                                                    },
                                                }
                                                }
                                            }
                                        }
                                        Some(CacheableMethod::ToolsList) => {
                                            match prepare_tools_list_request(
                                                &handshake_cache,
                                                &client_id,
                                                original_id.clone(),
                                            ) {
                                                DiscoveryAction::Cached(response) => {
                                                    diagnostics::log(format!(
                                                        "pool_cache_hit method=tools/list client_id={}",
                                                        client_id
                                                    ));
                                                    ClientAction::Cached(response)
                                                }
                                                DiscoveryAction::Coalesced => {
                                                    diagnostics::log(format!(
                                                        "pool_tools_list_coalesced client_id={}",
                                                        client_id
                                                    ));
                                                    ClientAction::Drop
                                                }
                                                DiscoveryAction::Leader => {
                                                diagnostics::log(format!(
                                                    "pool_cache_miss method=tools/list client_id={}",
                                                    client_id
                                                ));
                                                let pool_id = id_allocator.allocate();
                                                request_map.lock().insert(
                                                    jsonrpc::id_key(&Value::from(pool_id)),
                                                    PendingRequestInfo {
                                                        client_id: client_id.clone(),
                                                        original_id,
                                                        method: Some("tools/list".to_string()),
                                                        cache_key,
                                                        tool: None,
                                                        inserted_at: Instant::now(),
                                                    },
                                                );
                                                *last_active_client.lock() = Some(client_id.clone());
                                                match value.clone() {
                                                    Value::Object(object) => ClientAction::Forward {
                                                        line: jsonrpc::with_id(object, Value::from(pool_id)),
                                                        method: Some("tools/list".to_string()),
                                                        tool: None,
                                                        pool_id: Some(pool_id),
                                                    },
                                                    _ => ClientAction::Forward {
                                                        line: line.clone(),
                                                        method: Some("tools/list".to_string()),
                                                        tool: None,
                                                        pool_id: Some(pool_id),
                                                    },
                                                }
                                                }
                                            }
                                        }
                                        None => {
                                        let pool_id = id_allocator.allocate();
                                        let forward_method = method.clone();
                                        // Store the real method so the response
                                        // route log reports the actual method
                                        // (e.g. tools/call) rather than `?`.
                                        let pending_method = method.clone();
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
                                                method: pending_method,
                                                cache_key,
                                                tool: tool.clone(),
                                                inserted_at: Instant::now(),
                                            },
                                        );
                                        // Record this client as most-recently-active
                                        // so a server-initiated callback can route
                                        // back to it (see route_server_request).
                                        *last_active_client.lock() = Some(client_id.clone());
                                        match value.clone() {
                                            Value::Object(object) => ClientAction::Forward {
                                                line: jsonrpc::with_id(object, Value::from(pool_id)),
                                                method: forward_method,
                                                tool: tool.clone(),
                                                pool_id: Some(pool_id),
                                            },
                                            // Unreachable: guarded by is_object
                                            // above, but match instead of unwrap to
                                            // stay panic-free.
                                            _ => ClientAction::Forward {
                                                line: line.clone(),
                                                method: forward_method,
                                                tool: tool.clone(),
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
                                            line: line.clone(),
                                            method,
                                            tool: None,
                                            pool_id: None,
                                        }
                                    }
                                }
                            }
                        }
                        Ok(_) => ClientAction::Forward {
                            line: line.clone(),
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
                                line: line.clone(),
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

                    // Wait for the upstream sender if it is still starting rather
                    // than dropping this request: the first initialize/tools-list
                    // must survive cold start for tools to appear promptly.
                    let sender = acquire_request_sender(
                        &request_tx,
                        &upstream_ready,
                        &shutdown,
                        &client_id,
                    )
                    .await;
                    if let Some(sender) = sender {
                        if sender.send(forward_line).await.is_err() {
                            diagnostics::log(format!(
                                "pool_request_forward_failed client_id={} reason=upstream_closed",
                                client_id
                            ));
                            break;
                        }
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
                            line.len()
                        ));
                    } else {
                        diagnostics::log(format!(
                            "pool_request_dropped client_id={} reason=upstream_unavailable",
                            client_id
                        ));
                    }
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
        if let Initialization::InFlight { waiters } = &mut cache.initialize {
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
