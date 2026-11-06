use std::time::Duration;

use anyhow::Result;

use super::McpClient;
use crate::config::ServerDef;
use crate::server_config::ConfiguredServer;

#[path = "pool_identity.rs"]
mod identity;
pub(crate) use identity::pool_name;

/// Credentials and identity are resolved once; reconnects reuse the same pool view.
pub(crate) struct PreparedEndpoint {
    name: String,
    definition: ServerDef,
}

impl PreparedEndpoint {
    pub(crate) async fn prepare(server: &ConfiguredServer, no_oauth: bool) -> Result<Self> {
        let definition = crate::oauth::prepare(server, no_oauth).await?;
        let name = pool_name(server, &definition)?;
        Ok(Self { name, definition })
    }

    pub(crate) fn timeout_ms(&self) -> Option<u64> {
        self.definition.timeout_ms
    }

    /// Preparation precedes this budget. Callers may wrap both in a total budget.
    pub(crate) async fn connect(&self, deadline: Duration) -> Result<McpClient> {
        McpClient::connect(&self.name, &self.definition, deadline).await
    }
}
