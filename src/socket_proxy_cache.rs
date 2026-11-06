use super::*;

fn complete_cached_response(
    value: &Value,
    leader: PendingRequestInfo,
    completion: crate::mcp_session::SharedRequestCompletion,
    log: &diagnostics::ServerLog,
) -> Vec<(String, String)> {
    match completion.cache_event {
        Some(crate::mcp_session::CacheEvent::Stored(method)) => {
            log.log(format!("pool_cache_stored method={method}"));
        }
        Some(crate::mcp_session::CacheEvent::DiscoveryStored { modern_supported }) => {
            log.log(format!(
                "pool_cache_stored method=server/discover modern_supported={modern_supported}"
            ));
        }
        Some(crate::mcp_session::CacheEvent::DiscoveryUnsupported) => {
            log.log("pool_discovery_unsupported_cached");
        }
        None => {}
    }
    let waiters = completion.waiters;
    let mut responses = Vec::with_capacity(waiters.len() + 1);
    if let Value::Object(object) = value {
        log.log(format!(
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
    routes: &SessionRoutes,
    recovery_tx: &mpsc::Sender<RecoveryReason>,
) {
    if !is_session_not_found_error(value) {
        return;
    }
    routes.clear_handshake();
    if recovery_tx
        .try_send(RecoveryReason::SessionNotFound)
        .is_err()
    {
        routes
            .diagnostics
            .log("pool_recovery_signal_failed reason=channel_unavailable");
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

pub(super) enum RouteAdmission {
    Cached(String),
    Coalesced,
    Leader(u64),
}

/// Owns pending routes and their shared handshake transitions for one generation.
/// Upstream queue publication is synchronous under the route lock; delivery never is.
pub(super) struct SessionRoutes {
    requests: RequestMap,
    cache: Mutex<HandshakeCache>,
    id_allocator: IdAllocator,
    diagnostics: diagnostics::ServerLog,
}

impl SessionRoutes {
    pub fn new(diagnostics: diagnostics::ServerLog) -> Self {
        Self {
            diagnostics,
            requests: Mutex::new(HashMap::new()),
            cache: Mutex::new(HandshakeCache::default()),
            id_allocator: IdAllocator::new(),
        }
    }

    pub fn admit(
        &self,
        pending: PendingRequestInfo,
        transport_deadline: Instant,
    ) -> RouteAdmission {
        let mut requests = self.requests.lock();
        let mut cache = self.cache.lock();
        let action = pending.cache_key.map_or(DiscoveryAction::Leader, |method| {
            cache.prepare(
                method,
                PendingWaiter {
                    client_id: pending.client_id.clone(),
                    original_id: pending.original_id.clone(),
                    inserted_at: pending.inserted_at,
                    expires_after: pending.expires_after,
                },
                transport_deadline,
            )
        });
        match action {
            DiscoveryAction::Cached(response) => RouteAdmission::Cached(response),
            DiscoveryAction::Coalesced => RouteAdmission::Coalesced,
            DiscoveryAction::Leader => {
                let pool_id = self.id_allocator.allocate();
                requests.insert(jsonrpc::id_key(&Value::from(pool_id)), pending);
                RouteAdmission::Leader(pool_id)
            }
        }
    }

    pub fn contains(&self, pool_id: u64) -> bool {
        self.requests
            .lock()
            .contains_key(&jsonrpc::id_key(&Value::from(pool_id)))
    }

    pub fn publish(
        &self,
        pool_id: Option<u64>,
        shutdown: &AtomicBool,
        permit: mpsc::OwnedPermit<crate::upstream::UpstreamRequest>,
        line: String,
    ) -> bool {
        let requests = self.requests.lock();
        if shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let deadline = if let Some(pool_id) = pool_id {
            let Some(pending) = requests.get(&jsonrpc::id_key(&Value::from(pool_id))) else {
                return false;
            };
            pending
                .cache_key
                .and_then(|method| self.cache.lock().deadline(method))
        } else {
            None
        };
        permit.send(crate::upstream::UpstreamRequest { line, deadline });
        true
    }

    pub fn complete(&self, id: &Value, value: &Value) -> Option<Vec<(String, String)>> {
        let (pending, shared) = {
            let mut requests = self.requests.lock();
            let pending = requests.remove(&jsonrpc::id_key(id))?;
            let shared = pending
                .cache_key
                .map(|method| self.cache.lock().complete(method, value));
            (pending, shared)
        };
        Some(match shared {
            Some(completion) => {
                complete_cached_response(value, pending, completion, &self.diagnostics)
            }
            None => {
                self.diagnostics.log(format!(
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
        })
    }

    pub fn disconnect(&self, client_id: &str) {
        let mut requests = self.requests.lock();
        self.cache.lock().disconnect(&mut requests, client_id);
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        let requests = self.requests.lock();
        self.cache.lock().next_deadline(requests.values())
    }

    pub fn expire(&self, now: Instant) -> Vec<(String, String)> {
        let (responses, removed, remaining) = {
            let mut requests = self.requests.lock();
            let before = requests.len();
            let responses = self.cache.lock().expire(&mut requests, now);
            (responses, before - requests.len(), requests.len())
        };
        if removed > 0 {
            self.diagnostics.log(format!(
                "pool_stale_requests_cleaned removed={removed} remaining={remaining}"
            ));
        }
        responses
    }

    pub fn clear(&self) {
        let mut requests = self.requests.lock();
        let mut cache = self.cache.lock();
        requests.clear();
        cache.clear_all();
    }

    fn clear_handshake(&self) {
        let _requests = self.requests.lock();
        self.cache.lock().clear_all();
    }

    pub fn initialization_ready(&self) -> bool {
        self.cache.lock().initialization_ready()
    }

    pub fn swallow_initialized(&self, value: &Value) -> bool {
        self.cache.lock().swallow_initialized(value)
    }

    pub fn invalidate_tools_list(&self) {
        self.cache.lock().invalidate_tools_list();
    }
}

#[cfg(test)]
#[path = "socket_proxy_route_test_clock.rs"]
mod test_clock;
