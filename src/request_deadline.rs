use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::watch;

pub const TIMEOUT_FIELD: &str = "_mcp_pool_timeout_ms";

#[derive(Debug, Clone)]
pub struct SharedDeadline(watch::Sender<Instant>);

impl SharedDeadline {
    pub fn new(deadline: Instant) -> Self {
        Self(watch::channel(deadline).0)
    }

    pub fn current(&self) -> Instant {
        *self.0.borrow()
    }

    pub fn extend(&self, deadline: Instant) {
        self.0.send_if_modified(|current| {
            if deadline > *current {
                *current = deadline;
                true
            } else {
                false
            }
        });
    }

    pub async fn wait<F: std::future::Future>(&self, future: F) -> Result<F::Output, ()> {
        let mut changes = self.0.subscribe();
        tokio::pin!(future);
        loop {
            let deadline = *changes.borrow_and_update();
            tokio::select! {
                biased;
                result = &mut future => return Ok(result),
                changed = changes.changed() => {
                    if changed.is_err() {
                        return tokio::time::timeout_at(deadline.into(), &mut future)
                            .await.map_err(|_| ());
                    }
                }
                _ = tokio::time::sleep_until(deadline.into()) => {
                    if Instant::now() >= self.current() {
                        return Err(());
                    }
                }
            }
        }
    }
}

pub fn timeout_ms(message: &Value) -> Result<Option<u64>, String> {
    message
        .get(TIMEOUT_FIELD)
        .map(|value| {
            let milliseconds = value
                .as_u64()
                .filter(|milliseconds| *milliseconds > 0)
                .ok_or("Pool request timeout must be positive u64 milliseconds")?;
            Instant::now()
                .checked_add(Duration::from_millis(milliseconds))
                .ok_or("Pool request timeout exceeds the clock range")?;
            Ok(milliseconds)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_absent_and_long_deadlines() -> Result<(), String> {
        assert_eq!(timeout_ms(&json!({"method":"ping"}))?, None);
        assert_eq!(timeout_ms(&json!({TIMEOUT_FIELD: 600_000}))?, Some(600_000));
        Ok(())
    }

    #[test]
    fn rejects_invalid_deadlines() {
        for value in [json!(0), json!(-1), json!(1.5), json!("600"), Value::Null] {
            assert!(timeout_ms(&json!({TIMEOUT_FIELD: value})).is_err());
        }
    }
}
