use anyhow::{Context, Result};
use serde_json::Value;

use crate::server_config::{ConfiguredServer, ServerConfiguration};

pub(super) async fn inspect(
    configuration: &ServerConfiguration,
) -> Result<Vec<(&ConfiguredServer, Value)>> {
    inspect_with(configuration, crate::oauth::credential_status).await
}

async fn inspect_with<Inspector>(
    configuration: &ServerConfiguration,
    inspector: Inspector,
) -> Result<Vec<(&ConfiguredServer, Value)>>
where
    Inspector: Fn(&ConfiguredServer) -> Result<Value> + Clone + Send + 'static,
{
    let mut servers = configuration.servers.values();
    let mut credentials = Vec::new();
    loop {
        let mut tasks = Vec::new();
        for server in servers.by_ref().take(4) {
            let selected = server.clone();
            let inspect = inspector.clone();
            tasks.push((
                server,
                tokio::task::spawn_blocking(move || inspect(&selected)),
            ));
        }
        if tasks.is_empty() {
            return Ok(credentials);
        }
        let mut first_error = None;
        for (server, task) in tasks {
            match task
                .await
                .context("Credential inspection task failed")
                .and_then(|status| status)
            {
                Ok(status) => credentials.push((server, status)),
                Err(error) if first_error.is_none() => first_error = Some(error),
                Err(_) => {}
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
    }
}

#[cfg(test)]
#[path = "config_doctor_tests.rs"]
mod tests;
