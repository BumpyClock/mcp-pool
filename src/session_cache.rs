use std::collections::HashMap;
use std::time::Instant;

use serde_json::Value;

use super::{
    CacheEvent, CacheableMethod, DiscoverySupport, HandshakeCache, Initialization,
    PendingRequestInfo, PendingWaiter, SharedRequestAction, SharedRequestCompletion,
    build_error_response, build_success_response, cache_deadline,
    is_definitive_discovery_unsupported,
};
use crate::request_deadline::SharedDeadline;

#[derive(Debug, Default)]
pub(super) struct RequestCache {
    cached_result: Option<Value>,
    cached_until: Option<Instant>,
    invalidated_in_flight: bool,
    waiters: Vec<PendingWaiter>,
    in_flight: Option<SharedDeadline>,
}

impl RequestCache {
    pub(super) fn get(&self) -> Option<Value> {
        if self.expired() {
            None
        } else {
            self.cached_result.clone()
        }
    }

    pub(super) fn store(&mut self, result: Value) {
        self.cached_until = cache_deadline(&result);
        self.cached_result = (!self.expired()).then_some(result);
    }

    pub(super) fn invalidate(&mut self) {
        self.cached_result = None;
        self.cached_until = None;
        self.invalidated_in_flight = self.in_flight.is_some();
    }

    pub(super) fn clear_expired(&mut self) {
        if self.expired() {
            self.cached_result = None;
            self.cached_until = None;
        }
    }

    pub(super) fn deadline(&self) -> Option<SharedDeadline> {
        self.in_flight.clone()
    }

    fn prepare(
        &mut self,
        waiter: PendingWaiter,
        transport_deadline: Instant,
    ) -> SharedRequestAction {
        if let Some(result) = self.get() {
            return SharedRequestAction::Cached(build_success_response(waiter.original_id, result));
        }
        if let Some(deadline) = &self.in_flight {
            deadline.extend(transport_deadline);
            self.waiters.push(waiter);
            SharedRequestAction::Coalesced
        } else {
            self.invalidated_in_flight = false;
            self.in_flight = Some(SharedDeadline::new(transport_deadline));
            SharedRequestAction::Leader
        }
    }

    fn finish(&mut self) -> (Vec<PendingWaiter>, bool) {
        self.in_flight = None;
        (
            std::mem::take(&mut self.waiters),
            std::mem::take(&mut self.invalidated_in_flight),
        )
    }

    fn expired(&self) -> bool {
        self.cached_until
            .is_some_and(|deadline| Instant::now() >= deadline)
    }
}

impl HandshakeCache {
    pub fn prepare(
        &mut self,
        method: CacheableMethod,
        waiter: PendingWaiter,
        transport_deadline: Instant,
    ) -> SharedRequestAction {
        self.clear_expired_results();
        let rejection = match method {
            CacheableMethod::Initialize
                if self.server_discovery.support == DiscoverySupport::Modern
                    || self.server_discovery.requests.in_flight.is_some() =>
            {
                Some(
                    "initialize is unavailable because modern server discovery selected the upstream protocol",
                )
            }
            CacheableMethod::ServerDiscover
                if self.server_discovery.support == DiscoverySupport::Unsupported =>
            {
                Some("server/discover is unsupported by this upstream; use legacy initialize")
            }
            CacheableMethod::ServerDiscover
                if !matches!(self.initialize, Initialization::Empty) =>
            {
                Some("server/discover is unavailable after a legacy initialize handshake")
            }
            _ => None,
        };
        if let Some(message) = rejection {
            return SharedRequestAction::Cached(build_error_response(
                waiter.original_id,
                -32601,
                message,
            ));
        }
        match method {
            CacheableMethod::Initialize => match &mut self.initialize {
                Initialization::Ready { result, .. } => SharedRequestAction::Cached(
                    build_success_response(waiter.original_id, result.clone()),
                ),
                Initialization::InFlight { waiters, deadline } => {
                    deadline.extend(transport_deadline);
                    waiters.push(waiter);
                    SharedRequestAction::Coalesced
                }
                Initialization::Empty => {
                    self.initialize = Initialization::InFlight {
                        waiters: Vec::new(),
                        deadline: SharedDeadline::new(transport_deadline),
                    };
                    SharedRequestAction::Leader
                }
            },
            CacheableMethod::ServerDiscover => self
                .server_discovery
                .requests
                .prepare(waiter, transport_deadline),
            CacheableMethod::ToolsList => self.tools_list.prepare(waiter, transport_deadline),
        }
    }

    pub fn complete(&mut self, method: CacheableMethod, value: &Value) -> SharedRequestCompletion {
        let (waiters, invalidated) = self.finish(method);
        let result = value.get("result").filter(|_| value.get("error").is_none());
        let mut cache_event = None;
        match method {
            CacheableMethod::Initialize => {
                if let Some(result) = result {
                    self.store_result("initialize", result.clone());
                    cache_event = Some(CacheEvent::Stored("initialize"));
                }
            }
            CacheableMethod::ServerDiscover => {
                if let Some(result) = result
                    && let Ok(modern_supported) =
                        crate::mcp_protocol::discovery_offers_modern(result)
                {
                    self.store_result("server/discover", result.clone());
                    cache_event = Some(CacheEvent::DiscoveryStored { modern_supported });
                } else if is_definitive_discovery_unsupported(value) {
                    self.server_discovery.support = DiscoverySupport::Unsupported;
                    self.server_discovery.requests.invalidate();
                    cache_event = Some(CacheEvent::DiscoveryUnsupported);
                }
            }
            CacheableMethod::ToolsList => {
                if let Some(result) = result
                    && !invalidated
                {
                    self.store_result("tools/list", result.clone());
                    cache_event = Some(CacheEvent::Stored("tools/list"));
                }
            }
        }
        SharedRequestCompletion {
            waiters,
            cache_event,
        }
    }

    pub fn disconnect(
        &mut self,
        requests: &mut HashMap<String, PendingRequestInfo>,
        client_id: &str,
    ) {
        requests.retain(|_, pending| pending.client_id != client_id || pending.cache_key.is_some());
        if let Initialization::InFlight { waiters, .. } = &mut self.initialize {
            waiters.retain(|waiter| waiter.client_id != client_id);
        }
        self.server_discovery
            .requests
            .waiters
            .retain(|waiter| waiter.client_id != client_id);
        self.tools_list
            .waiters
            .retain(|waiter| waiter.client_id != client_id);
    }

    pub fn initialization_ready(&self) -> bool {
        matches!(self.initialize, Initialization::Ready { .. })
    }

    fn pending_expiration(&self, pending: &PendingRequestInfo) -> Instant {
        let deadline = expiration(pending.inserted_at, pending.expires_after);
        pending
            .cache_key
            .and_then(|method| self.deadline(method))
            .map_or(deadline, |shared| deadline.max(shared.current()))
    }

    pub fn next_deadline<'a>(
        &self,
        requests: impl Iterator<Item = &'a PendingRequestInfo>,
    ) -> Option<Instant> {
        requests
            .map(|pending| self.pending_expiration(pending))
            .chain(
                self.tools_list
                    .waiters
                    .iter()
                    .chain(&self.server_discovery.requests.waiters)
                    .map(|waiter| expiration(waiter.inserted_at, waiter.expires_after)),
            )
            .min()
    }

    pub fn expire(
        &mut self,
        requests: &mut HashMap<String, PendingRequestInfo>,
        now: Instant,
    ) -> Vec<(String, String)> {
        let stale_keys: Vec<String> = requests
            .iter()
            .filter(|(_, pending)| now >= self.pending_expiration(pending))
            .map(|(key, _)| key.clone())
            .collect();
        let mut stale_requests = Vec::with_capacity(stale_keys.len());
        for key in stale_keys {
            if let Some(request) = requests.remove(&key) {
                stale_requests.push(request);
            }
        }
        let mut responses = Vec::new();
        for method in [
            CacheableMethod::Initialize,
            CacheableMethod::ServerDiscover,
            CacheableMethod::ToolsList,
        ] {
            if stale_requests
                .iter()
                .any(|pending| pending.cache_key == Some(method))
            {
                responses.extend(
                    stale_requests
                        .iter()
                        .filter(|pending| pending.cache_key == Some(method))
                        .map(|pending| {
                            timeout_response(
                                pending.client_id.clone(),
                                pending.original_id.clone(),
                                method.timeout_message(),
                            )
                        }),
                );
                responses.extend(self.finish(method).0.into_iter().map(|waiter| {
                    timeout_response(
                        waiter.client_id,
                        waiter.original_id,
                        method.timeout_message(),
                    )
                }));
            }
        }
        responses.extend(expire_waiters(
            &mut self.tools_list.waiters,
            CacheableMethod::ToolsList,
            now,
        ));
        responses.extend(expire_waiters(
            &mut self.server_discovery.requests.waiters,
            CacheableMethod::ServerDiscover,
            now,
        ));
        responses.extend(
            stale_requests
                .into_iter()
                .filter(|pending| pending.cache_key.is_none())
                .map(|pending| {
                    timeout_response(pending.client_id, pending.original_id, "request timed out")
                }),
        );
        responses
    }

    fn finish(&mut self, method: CacheableMethod) -> (Vec<PendingWaiter>, bool) {
        match method {
            CacheableMethod::Initialize => match std::mem::take(&mut self.initialize) {
                Initialization::InFlight { waiters, .. } => (waiters, false),
                state => {
                    self.initialize = state;
                    (Vec::new(), false)
                }
            },
            CacheableMethod::ServerDiscover => self.server_discovery.requests.finish(),
            CacheableMethod::ToolsList => self.tools_list.finish(),
        }
    }

    #[cfg(test)]
    pub fn waiter_count(&self, method: CacheableMethod) -> usize {
        match method {
            CacheableMethod::Initialize => match &self.initialize {
                Initialization::InFlight { waiters, .. } => waiters.len(),
                _ => 0,
            },
            CacheableMethod::ServerDiscover => self.server_discovery.requests.waiters.len(),
            CacheableMethod::ToolsList => self.tools_list.waiters.len(),
        }
    }
}

#[cfg(test)]
#[path = "session_cache_test_clock.rs"]
mod test_clock;

fn expiration(inserted_at: Instant, expires_after: std::time::Duration) -> Instant {
    inserted_at
        .checked_add(expires_after)
        .unwrap_or_else(Instant::now)
}

fn timeout_response(client_id: String, original_id: Value, message: &str) -> (String, String) {
    (
        client_id,
        build_error_response(original_id, -32001, message),
    )
}

fn expire_waiters(
    waiters: &mut Vec<PendingWaiter>,
    method: CacheableMethod,
    now: Instant,
) -> Vec<(String, String)> {
    let mut stale = Vec::new();
    waiters.retain(|waiter| {
        if now >= expiration(waiter.inserted_at, waiter.expires_after) {
            stale.push(timeout_response(
                waiter.client_id.clone(),
                waiter.original_id.clone(),
                method.timeout_message(),
            ));
            false
        } else {
            true
        }
    });
    stale
}
