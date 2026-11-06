use super::*;
use crate::request_deadline::SharedDeadline;

impl SessionRoutes {
    pub fn pending_count(&self) -> usize {
        self.requests.lock().len()
    }

    pub fn cached_result(&self, method: &str) -> Option<Value> {
        self.cache.lock().get(method)
    }

    pub fn shared_deadline(&self, method: CacheableMethod) -> Option<SharedDeadline> {
        self.cache.lock().deadline(method)
    }

    pub fn waiter_count(&self, method: CacheableMethod) -> usize {
        self.cache.lock().waiter_count(method)
    }

    pub fn elapse_requests(&self, elapsed: Duration) {
        for pending in self.requests.lock().values_mut() {
            if let Some(shifted) = pending.inserted_at.checked_sub(elapsed) {
                pending.inserted_at = shifted;
            }
        }
    }

    pub fn elapse_waiters(&self, method: CacheableMethod, elapsed: Duration) {
        self.cache.lock().elapse_waiters(method, elapsed);
    }

    pub fn expire_requests_except(&self, keep: Option<CacheableMethod>, remaining: Duration) {
        let expiry = Instant::now() + remaining;
        let mut requests = self.requests.lock();
        let mut cache = self.cache.lock();
        for pending in requests.values_mut() {
            if keep.is_some() && pending.cache_key == keep {
                continue;
            }
            pending.inserted_at = expiry - pending.expires_after;
            if let Some(method) = pending.cache_key
                && let Some(deadline) = cache.deadline(method)
            {
                cache.elapse_shared_wait(
                    method,
                    deadline.current().saturating_duration_since(expiry),
                );
            }
        }
    }

    pub fn elapse_cached_results(&self, elapsed: Duration) {
        self.cache.lock().elapse_results(elapsed);
    }
}
