use super::*;
use std::time::Duration;

impl HandshakeCache {
    pub fn elapse_results(&mut self, elapsed: Duration) {
        for request in [&mut self.server_discovery.requests, &mut self.tools_list] {
            request.cached_until = request
                .cached_until
                .map(|deadline| deadline.checked_sub(elapsed).unwrap_or_else(Instant::now));
        }
    }

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
            CacheableMethod::ServerDiscover => {
                match &mut self.server_discovery.requests.in_flight {
                    Some(in_flight) => &mut in_flight.waiters,
                    None => return,
                }
            }
            CacheableMethod::ToolsList => match &mut self.tools_list.in_flight {
                Some(in_flight) => &mut in_flight.waiters,
                None => return,
            },
        };
        for waiter in waiters {
            if let Some(shifted) = waiter.inserted_at.checked_sub(elapsed) {
                waiter.inserted_at = shifted;
            }
        }
    }
}

fn elapse_request(request: &mut RequestCache, elapsed: Duration) {
    if let Some(in_flight) = &mut request.in_flight
        && let Some(shifted) = in_flight.deadline.current().checked_sub(elapsed)
    {
        in_flight.deadline = SharedDeadline::new(shifted);
    }
}
