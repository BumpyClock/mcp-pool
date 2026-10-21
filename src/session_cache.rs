use std::time::Instant;

use serde_json::Value;

use super::{PendingWaiter, cache_deadline};
use crate::request_deadline::SharedDeadline;

#[derive(Debug, Default)]
pub struct RequestCache {
    pub cached_result: Option<Value>,
    pub cached_until: Option<Instant>,
    pub invalidated_in_flight: bool,
    pub waiters: Vec<PendingWaiter>,
    pub in_flight: Option<SharedDeadline>,
}

impl RequestCache {
    pub fn get(&self) -> Option<Value> {
        if self.expired() {
            None
        } else {
            self.cached_result.clone()
        }
    }

    pub fn store(&mut self, result: Value) {
        self.cached_until = cache_deadline(&result);
        self.cached_result = (!self.expired()).then_some(result);
    }

    pub fn invalidate(&mut self) {
        self.cached_result = None;
        self.cached_until = None;
        self.invalidated_in_flight = self.in_flight.is_some();
    }

    pub fn clear_expired(&mut self) {
        if self.expired() {
            self.cached_result = None;
            self.cached_until = None;
        }
    }

    pub fn coalesce(&mut self, waiter: PendingWaiter, transport_deadline: Instant) -> bool {
        if let Some(deadline) = &self.in_flight {
            deadline.extend(transport_deadline);
            self.waiters.push(waiter);
            true
        } else {
            self.invalidated_in_flight = false;
            self.in_flight = Some(SharedDeadline::new(transport_deadline));
            false
        }
    }

    pub fn finish(&mut self) -> (Vec<PendingWaiter>, bool) {
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
