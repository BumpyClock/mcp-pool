use anyhow::{Context, Result};
use parking_lot::Mutex;
use tokio::sync::{Semaphore, SemaphorePermit};

use super::{ClientFuture, ReconnectFactory};
use crate::mcp_client::McpClient;

pub(super) struct Clients {
    idle: Mutex<Vec<Option<McpClient>>>,
    available: Semaphore,
}

struct Lease<'a> {
    client: Option<McpClient>,
    pool: &'a Clients,
    _permit: SemaphorePermit<'a>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.pool.idle.lock().push(self.client.take());
    }
}

impl Clients {
    pub(super) fn new(client: McpClient, concurrency: usize) -> Self {
        let mut idle = (1..concurrency).map(|_| None).collect::<Vec<_>>();
        idle.push(Some(client));
        Self {
            idle: Mutex::new(idle),
            available: Semaphore::new(concurrency),
        }
    }

    pub(super) async fn with_client<T>(
        &self,
        reconnect: &ReconnectFactory,
        operation: impl for<'a> FnOnce(&'a mut McpClient) -> ClientFuture<'a, T>,
    ) -> Result<T> {
        let permit = self.available.acquire().await?;
        let client = self
            .idle
            .lock()
            .pop()
            .context("Bridge connection slot unavailable after admission")?;
        let mut lease = Lease {
            client,
            pool: self,
            _permit: permit,
        };
        if lease.client.as_ref().is_none_or(McpClient::is_closed) {
            lease.client = Some(reconnect().await?);
        }
        operation(
            lease
                .client
                .as_mut()
                .context("Bridge connection was not established")?,
        )
        .await
    }
}
