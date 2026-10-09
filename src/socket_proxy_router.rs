use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn route_response(
    line: &str,
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    request_map: &RequestMap,
    handshake_cache: &HandshakeCacheRef,
    last_active_client: &Arc<Mutex<Option<String>>>,
    client_capabilities: &Arc<Mutex<HashMap<String, ClientCapabilities>>>,
    request_tx: &Arc<Mutex<Option<mpsc::Sender<crate::upstream::UpstreamRequest>>>>,
    recovery_tx: &mpsc::Sender<RecoveryReason>,
) {
    match serde_json::from_str::<Value>(line) {
        Ok(value) if value.is_object() => {
            let id = non_null_id(&value);
            match (message_has_method(&value), id) {
                (true, Some(_)) => {
                    route_server_request(
                        line,
                        &value,
                        clients,
                        last_active_client,
                        client_capabilities,
                        request_tx,
                    )
                    .await;
                }
                (false, Some(id)) => {
                    let key = jsonrpc::id_key(id);
                    let pending = request_map.lock().remove(&key);
                    if let Some(pending) = pending {
                        for (client_id, payload) in
                            restore_response(&value, pending, handshake_cache)
                        {
                            send_to_client(&client_id, payload, clients).await;
                        }
                        request_recovery_if_session_not_found(&value, handshake_cache, recovery_tx);
                    } else {
                        diagnostics::log(format!(
                            "pool_response_orphaned id={key} reason=no_pending_request"
                        ));
                    }
                }
                (false, None) => {
                    diagnostics::log("pool_response_orphaned reason=uncorrelated_response");
                }
                (true, None) => {
                    if value.get("method").and_then(Value::as_str)
                        == Some("notifications/tools/list_changed")
                    {
                        handshake_cache.lock().invalidate_tools_list();
                        diagnostics::log("pool_tools_cache_invalidated");
                    }
                    broadcast_to_all(line, clients).await;
                }
            }
        }
        Ok(_) => broadcast_to_all(line, clients).await,
        Err(_) => {
            diagnostics::log(format!("pool_response_parse_failed bytes={}", line.len()));
            broadcast_to_all(line, clients).await;
        }
    }
}

fn restore_response(
    value: &Value,
    pending: PendingRequestInfo,
    cache: &HandshakeCacheRef,
) -> Vec<(String, String)> {
    match pending.cache_key {
        Some(CacheableMethod::Initialize) => complete_initialize_response(value, pending, cache),
        Some(CacheableMethod::ToolsList) => complete_tools_list_response(value, pending, cache),
        None => {
            diagnostics::log(format!(
                "pool_response_routed client_id={} method={}{} elapsed_ms={} outcome={}",
                pending.client_id,
                pending.method.as_deref().unwrap_or("?"),
                pending
                    .tool
                    .as_deref()
                    .map(|tool| format!(" tool={tool}"))
                    .unwrap_or_default(),
                pending.inserted_at.elapsed().as_millis(),
                response_outcome(value),
            ));
            match value {
                Value::Object(object) => vec![(
                    pending.client_id,
                    jsonrpc::with_id(object.clone(), pending.original_id),
                )],
                _ => Vec::new(),
            }
        }
    }
}
