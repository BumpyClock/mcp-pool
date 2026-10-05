use super::*;

pub(super) fn prepare_initialize_request(
    cache: &HandshakeCacheRef,
    client_id: &str,
    original_id: Value,
) -> DiscoveryAction {
    let mut cache = cache.lock();
    match &mut cache.initialize {
        Initialization::Ready { result, .. } => {
            DiscoveryAction::Cached(build_success_response(original_id, result.clone()))
        }
        Initialization::InFlight { waiters } => {
            waiters.push(PendingWaiter {
                client_id: client_id.to_string(),
                original_id,
                inserted_at: Instant::now(),
            });
            DiscoveryAction::Coalesced
        }
        Initialization::Empty => {
            cache.initialize = Initialization::InFlight {
                waiters: Vec::new(),
            };
            DiscoveryAction::Leader
        }
    }
}

pub(super) fn complete_initialize_response(
    value: &Value,
    leader: PendingRequestInfo,
    cache: &HandshakeCacheRef,
) -> Vec<(String, String)> {
    let mut cache = cache.lock();
    let waiters = match std::mem::take(&mut cache.initialize) {
        Initialization::InFlight { waiters } => waiters,
        state => {
            cache.initialize = state;
            Vec::new()
        }
    };
    if value.get("error").is_none()
        && let Some(result) = value.get("result")
    {
        cache.store("initialize", result.clone());
        diagnostics::log("pool_cache_stored method=initialize");
    }
    let Value::Object(object) = value else {
        return Vec::new();
    };
    diagnostics::log(format!(
        "pool_response_routed client_id={} method=initialize elapsed_ms={} outcome={} waiters={}",
        leader.client_id,
        leader.inserted_at.elapsed().as_millis(),
        response_outcome(value),
        waiters.len(),
    ));
    let mut responses = Vec::with_capacity(waiters.len() + 1);
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
    responses
}

pub(super) fn cleanup_initialize_after_stale_requests(
    cache: &HandshakeCacheRef,
    stale_requests: &[PendingRequestInfo],
) -> Vec<(String, String)> {
    if !stale_requests
        .iter()
        .any(|pending| pending.cache_key == Some(CacheableMethod::Initialize))
    {
        return Vec::new();
    }
    let mut cache = cache.lock();
    let waiters = match std::mem::take(&mut cache.initialize) {
        Initialization::InFlight { waiters } => waiters,
        state => {
            cache.initialize = state;
            Vec::new()
        }
    };
    stale_requests
        .iter()
        .filter(|pending| pending.cache_key == Some(CacheableMethod::Initialize))
        .map(|pending| {
            (
                pending.client_id.clone(),
                build_error_response(pending.original_id.clone(), -32001, "initialize timed out"),
            )
        })
        .chain(waiters.into_iter().map(|waiter| {
            (
                waiter.client_id,
                build_error_response(waiter.original_id, -32001, "initialize timed out"),
            )
        }))
        .collect()
}

pub(super) fn prepare_tools_list_request(
    cache: &HandshakeCacheRef,
    client_id: &str,
    original_id: Value,
) -> DiscoveryAction {
    let mut cache = cache.lock();
    if let Some(result) = cache.get("tools/list") {
        return DiscoveryAction::Cached(build_success_response(original_id, result));
    }
    if cache.tools_list.in_flight {
        cache.tools_list.waiters.push(PendingWaiter {
            client_id: client_id.to_string(),
            original_id,
            inserted_at: Instant::now(),
        });
        return DiscoveryAction::Coalesced;
    }
    cache.tools_list.in_flight = true;
    DiscoveryAction::Leader
}

pub(super) fn complete_tools_list_response(
    value: &Value,
    leader: PendingRequestInfo,
    cache: &HandshakeCacheRef,
) -> Vec<(String, String)> {
    let mut cache = cache.lock();
    cache.tools_list.in_flight = false;
    let waiters = std::mem::take(&mut cache.tools_list.waiters);

    if value.get("error").is_none()
        && let Some(result) = value.get("result").cloned()
    {
        cache.tools_list.cached_result = Some(result);
        diagnostics::log("pool_cache_stored method=tools/list");
    }

    let mut responses = Vec::with_capacity(waiters.len() + 1);
    if let Value::Object(object) = value.clone() {
        diagnostics::log(format!(
            "pool_response_routed client_id={} method=tools/list elapsed_ms={} outcome={} waiters={}",
            leader.client_id,
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

pub(super) fn cleanup_stale_tools_list_waiters(cache: &HandshakeCacheRef) -> Vec<(String, String)> {
    let now = Instant::now();
    let mut cache = cache.lock();
    let mut stale = Vec::new();
    let mut kept = Vec::with_capacity(cache.tools_list.waiters.len());
    for waiter in cache.tools_list.waiters.drain(..) {
        if now.duration_since(waiter.inserted_at) >= Duration::from_secs(REQUEST_TTL_SECS) {
            stale.push((
                waiter.client_id,
                build_error_response(waiter.original_id, -32001, "tools/list discovery timed out"),
            ));
        } else {
            kept.push(waiter);
        }
    }
    cache.tools_list.waiters = kept;
    stale
}

pub(super) fn cleanup_tools_list_after_stale_requests(
    cache: &HandshakeCacheRef,
    stale_requests: &[PendingRequestInfo],
) -> Vec<(String, String)> {
    if !stale_requests
        .iter()
        .any(|pending| pending.cache_key == Some(CacheableMethod::ToolsList))
    {
        return Vec::new();
    }

    let mut cache = cache.lock();
    cache.tools_list.in_flight = false;
    let waiters = std::mem::take(&mut cache.tools_list.waiters);
    if !waiters.is_empty() {
        diagnostics::log(format!(
            "pool_tools_list_stale_leader_cleaned waiters={}",
            waiters.len()
        ));
    }

    waiters
        .into_iter()
        .map(|waiter| {
            (
                waiter.client_id,
                build_error_response(waiter.original_id, -32001, "tools/list discovery timed out"),
            )
        })
        .collect()
}

pub(super) fn request_recovery_if_session_not_found(
    value: &Value,
    cache: &HandshakeCacheRef,
    recovery_tx: &mpsc::Sender<RecoveryReason>,
    recovery_requested: &Arc<AtomicBool>,
) {
    if !is_session_not_found_error(value) {
        return;
    }
    cache.lock().clear_all();
    if recovery_requested.load(Ordering::SeqCst) {
        return;
    }
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

pub(super) fn cleanup_stale_requests(request_map: &RequestMap) -> Vec<PendingRequestInfo> {
    let now = Instant::now();
    let mut pending = request_map.lock();
    let stale_keys: Vec<String> = pending
        .iter()
        .filter_map(|(key, pending)| {
            if now.duration_since(pending.inserted_at) >= Duration::from_secs(REQUEST_TTL_SECS) {
                Some(key.clone())
            } else {
                None
            }
        })
        .collect();
    let mut removed_pending = Vec::with_capacity(stale_keys.len());
    for key in stale_keys {
        if let Some(request) = pending.remove(&key) {
            removed_pending.push(request);
        }
    }
    let after = pending.len();
    let removed = removed_pending.len();
    if removed > 0 {
        diagnostics::log(format!(
            "pool_stale_requests_cleaned removed={} remaining={}",
            removed, after
        ));
    }
    removed_pending
}

pub(super) fn expire_pending_requests(
    request_map: &RequestMap,
    cache: &HandshakeCacheRef,
) -> Vec<(String, String)> {
    let stale = cleanup_stale_requests(request_map);
    cleanup_tools_list_after_stale_requests(cache, &stale)
        .into_iter()
        .chain(cleanup_stale_tools_list_waiters(cache))
        .chain(cleanup_initialize_after_stale_requests(cache, &stale))
        .chain(
            stale
                .into_iter()
                .filter(|pending| pending.cache_key != Some(CacheableMethod::Initialize))
                .map(|pending| {
                    let message = if pending.cache_key == Some(CacheableMethod::ToolsList) {
                        "tools/list discovery timed out"
                    } else {
                        "request timed out"
                    };
                    (
                        pending.client_id,
                        build_error_response(pending.original_id, -32001, message),
                    )
                }),
        )
        .collect()
}
