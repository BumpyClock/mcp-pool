use super::*;

pub(super) fn complete_cached_response(
    value: &Value,
    leader: PendingRequestInfo,
    cache: &mut HandshakeCache,
    method: CacheableMethod,
) -> Vec<(String, String)> {
    let completion = cache.complete(method, value);
    match completion.cache_event {
        Some(crate::mcp_session::CacheEvent::Stored(method)) => {
            diagnostics::log(format!("pool_cache_stored method={method}"));
        }
        Some(crate::mcp_session::CacheEvent::DiscoveryStored { modern_supported }) => {
            diagnostics::log(format!(
                "pool_cache_stored method=server/discover modern_supported={modern_supported}"
            ));
        }
        Some(crate::mcp_session::CacheEvent::DiscoveryUnsupported) => {
            diagnostics::log("pool_discovery_unsupported_cached");
        }
        None => {}
    }
    let waiters = completion.waiters;
    let mut responses = Vec::with_capacity(waiters.len() + 1);
    if let Value::Object(object) = value {
        diagnostics::log(format!(
            "pool_response_routed client_id={} method={} elapsed_ms={} outcome={} waiters={}",
            leader.client_id,
            leader.method.as_deref().unwrap_or("?"),
            leader.inserted_at.elapsed().as_millis(),
            response_outcome(value),
            waiters.len()
        ));
        responses.push((
            leader.client_id,
            jsonrpc::with_id(object.clone(), leader.original_id),
        ));
        responses.extend(waiters.into_iter().map(|waiter| {
            (
                waiter.client_id,
                jsonrpc::with_id(object.clone(), waiter.original_id),
            )
        }));
    }
    responses
}

pub(super) fn request_recovery_if_session_not_found(
    value: &Value,
    cache: &Mutex<HandshakeCache>,
    recovery_tx: &mpsc::Sender<RecoveryReason>,
) {
    if !is_session_not_found_error(value) {
        return;
    }
    cache.lock().clear_all();
    if recovery_tx
        .try_send(RecoveryReason::SessionNotFound)
        .is_err()
    {
        diagnostics::log("pool_recovery_signal_failed reason=channel_unavailable");
    }
}

pub(super) fn response_outcome(value: &Value) -> &'static str {
    if value.get("error").is_some() {
        "error"
    } else {
        "result"
    }
}

pub(super) fn recovery_reason_label(reason: RecoveryReason) -> &'static str {
    match reason {
        RecoveryReason::SessionNotFound => "session_not_found",
    }
}

pub(super) fn expire_pending_requests(
    request_map: &RequestMap,
    cache: &Mutex<HandshakeCache>,
) -> Vec<(String, String)> {
    let now = Instant::now();
    let mut pending = request_map.lock();
    let mut cache = cache.lock();
    let before = pending.len();
    let responses = cache.expire(&mut pending, now);
    let removed = before - pending.len();
    if removed > 0 {
        diagnostics::log(format!(
            "pool_stale_requests_cleaned removed={} remaining={}",
            removed,
            pending.len(),
        ));
    }
    responses
}
