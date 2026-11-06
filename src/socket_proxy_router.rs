use super::*;

pub(super) async fn route_response(
    line: &str,
    generation: &Generation,
    recovery_tx: &mpsc::Sender<RecoveryReason>,
) {
    let Generation {
        clients,
        routes,
        diagnostics: log,
        ..
    } = generation;
    match serde_json::from_str::<Value>(line) {
        Ok(value) if value.is_object() => {
            let id = non_null_id(&value);
            match (message_has_method(&value), id) {
                (true, Some(id)) => {
                    route_server_request(line, &value, id, generation).await;
                }
                (false, Some(id)) => {
                    let key = jsonrpc::id_key(id);
                    let responses = routes.complete(id, &value);
                    if let Some(responses) = responses {
                        for (client_id, payload) in sweep_recipients(responses, clients, log) {
                            send_to_client(&client_id, payload, clients, log).await;
                        }
                        request_recovery_if_session_not_found(&value, routes, recovery_tx);
                    } else {
                        log.log(format!(
                            "pool_response_orphaned id={key} reason=no_pending_request"
                        ));
                    }
                }
                (false, None) => {
                    log.log("pool_response_orphaned reason=uncorrelated_response");
                }
                (true, None) => {
                    if value.get("method").and_then(Value::as_str)
                        == Some("notifications/tools/list_changed")
                    {
                        routes.invalidate_tools_list();
                        log.log("pool_tools_cache_invalidated");
                    }
                    broadcast_to_all(line, clients, log).await;
                }
            }
        }
        Ok(_) => broadcast_to_all(line, clients, log).await,
        Err(_) => {
            log.log(format!("pool_response_parse_failed bytes={}", line.len()));
            broadcast_to_all(line, clients, log).await;
        }
    }
}
