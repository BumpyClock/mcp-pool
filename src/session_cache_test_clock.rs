use super::*;
use std::time::Duration;

impl HandshakeCache {
    // Socket fixtures use the real expiration loop without waiting five minutes.
    // Keep clock manipulation here so they cannot construct inconsistent cache states.
    pub fn elapse_shared_wait(&mut self, method: CacheableMethod, elapsed: Duration) {
        match method {
            CacheableMethod::Initialize => {
                if let Initialization::InFlight { deadline, .. } = &mut self.initialize
                    && let Some(shifted) = deadline.current().checked_sub(elapsed)
                {
                    *deadline = SharedDeadline::new(shifted);
                }
            }
            CacheableMethod::ServerDiscover => {
                elapse_request(&mut self.server_discovery.requests, elapsed);
            }
            CacheableMethod::ToolsList => elapse_request(&mut self.tools_list, elapsed),
        }
    }

    pub fn elapse_waiters(&mut self, method: CacheableMethod, elapsed: Duration) {
        let waiters = match method {
            CacheableMethod::Initialize => match &mut self.initialize {
                Initialization::InFlight { waiters, .. } => waiters,
                _ => return,
            },
            CacheableMethod::ServerDiscover => &mut self.server_discovery.requests.waiters,
            CacheableMethod::ToolsList => &mut self.tools_list.waiters,
        };
        for waiter in waiters {
            if let Some(shifted) = waiter.inserted_at.checked_sub(elapsed) {
                waiter.inserted_at = shifted;
            }
        }
    }
}

fn elapse_request(request: &mut RequestCache, elapsed: Duration) {
    if let Some(deadline) = &mut request.in_flight
        && let Some(shifted) = deadline.current().checked_sub(elapsed)
    {
        *deadline = SharedDeadline::new(shifted);
    }
}
