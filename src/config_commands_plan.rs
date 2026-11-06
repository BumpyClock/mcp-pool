use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use super::write::WritePolicy;

#[derive(Clone, Copy)]
pub(super) enum AddPolicy {
    Regular,
    Copy { replace: bool },
}

pub(crate) struct EntryMutation {
    entries: Map<String, Value>,
    policy: AddPolicy,
}

pub(crate) struct Retirements {
    names: Vec<String>,
}

impl EntryMutation {
    pub(super) fn new(entries: Map<String, Value>, policy: AddPolicy) -> Self {
        Self { entries, policy }
    }

    pub(crate) fn import(entries: Map<String, Value>, replace: bool) -> Self {
        Self::new(entries, AddPolicy::Copy { replace })
    }

    pub(crate) fn write_policy(&self) -> WritePolicy {
        match self.policy {
            AddPolicy::Regular => WritePolicy::Replace,
            AddPolicy::Copy { .. } => WritePolicy::PreserveUnchanged,
        }
    }

    pub(crate) fn apply(&self, document: &mut Value, destination: &Path) -> Result<Retirements> {
        let servers = document
            .get_mut("mcpServers")
            .and_then(Value::as_object_mut)
            .context("Destination needs an mcpServers object")?;
        let mut names = Vec::new();
        for (name, entry) in &self.entries {
            if let Some(previous) = servers.get(name) {
                if previous != entry && matches!(self.policy, AddPolicy::Copy { replace: false }) {
                    bail!(
                        "Server '{name}' already exists in {}; use --replace to overwrite it. No changes made.",
                        destination.display()
                    );
                }
                if previous != entry || matches!(self.policy, AddPolicy::Regular) {
                    names.push(name.clone());
                }
            }
        }
        servers.extend(self.entries.clone());
        Ok(Retirements { names })
    }
}

impl Retirements {
    pub(crate) async fn retire(self, source: &Path) -> Result<()> {
        for name in self.names {
            crate::daemon_commands::retire_configuration(source, &name).await?;
        }
        Ok(())
    }
}
