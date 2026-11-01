use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};

use crate::config_import_sources::ImportBatch;
use crate::oauth::import::{Decision, PendingImport};
use crate::server_config::ConfiguredServer;

#[path = "config_import_auth_source.rs"]
mod source;

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Status {
    Import,
    Unchanged,
    Missing,
    Unsupported,
}

impl std::fmt::Display for Status {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Import => "import",
            Self::Unchanged => "unchanged",
            Self::Missing => "missing",
            Self::Unsupported => "unsupported",
        })
    }
}

#[derive(Default)]
pub(crate) struct Plan {
    pub statuses: BTreeMap<String, Status>,
    changes: Vec<(ConfiguredServer, Box<PendingImport>)>,
}

impl Plan {
    pub(crate) fn prepare(
        batch: &mut ImportBatch,
        destination: &Path,
        home: &Path,
        replace: bool,
    ) -> Result<Self> {
        let mut plan = Self::default();
        for (name, imported) in &mut batch.servers {
            let remote = imported
                .entry
                .as_object()
                .is_some_and(crate::server_config::is_remote_entry);
            let status = if remote {
                let credentials = source::read(name, imported, home)?;
                let entry = imported
                    .entry
                    .as_object_mut()
                    .context("Server must be an object")?;
                entry.remove("tokenCacheDir");
                entry.remove("token_cache_dir");
                let server = crate::server_config::authentication_server(
                    destination,
                    name,
                    &imported.entry,
                )?;
                let authentication = crate::oauth::context(&server)?;
                match PendingImport::prepare(&authentication, credentials, replace)? {
                    Decision::Missing => Status::Missing,
                    Decision::Unchanged => Status::Unchanged,
                    Decision::Import(mutation) => {
                        plan.changes.push((server, mutation));
                        Status::Import
                    }
                }
            } else {
                Status::Unsupported
            };
            plan.statuses.insert(name.clone(), status);
        }
        Ok(plan)
    }

    pub(crate) fn ensure_writable(&self) -> Result<()> {
        for (_, mutation) in &self.changes {
            mutation.ensure_writable()?;
        }
        Ok(())
    }

    pub(crate) async fn retire(&self) -> Result<()> {
        for (server, _) in &self.changes {
            crate::daemon_commands::retire_pool_views(server).await?;
        }
        Ok(())
    }

    pub(crate) fn commit(&self) -> Result<()> {
        for (server, mutation) in &self.changes {
            mutation.commit().with_context(|| format!(
                "Config saved, but OAuth credential import for '{}' failed; fix the store and rerun the import", server.name,
            ))?;
        }
        Ok(())
    }
}
