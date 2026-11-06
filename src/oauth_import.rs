use std::path::PathBuf;

use anyhow::{Result, bail};
use serde_json::Value;

use super::{HttpAuth, protocol, store};

#[cfg(test)]
#[path = "oauth_import_tests.rs"]
mod tests;

pub(crate) struct Source {
    snapshot: Value,
    files: Vec<PathBuf>,
}

impl Source {
    pub(crate) fn read(authentication: &HttpAuth) -> Result<Self> {
        let mut files = vec![authentication.vault_path.clone()];
        for directory in &authentication.directory_stores {
            files.extend(
                store::DIRECTORY_FIELDS
                    .iter()
                    .map(|(_, file, _)| directory.join(file)),
            );
            files.push(directory.join("server_url.txt"));
        }
        Ok(Self {
            snapshot: store::read(authentication)?,
            files,
        })
    }
}

pub(crate) enum Decision {
    Missing,
    Unchanged,
    Import(Box<PendingImport>),
}

pub(crate) struct PendingImport {
    destination: store::ImportDestination,
    payload: Value,
}

impl PendingImport {
    pub(crate) fn prepare(
        authentication: &HttpAuth,
        source: Source,
        replace: bool,
    ) -> Result<Decision> {
        let destination = crate::config_paths::absolute(&authentication.vault_path)?;
        for file in source.files {
            if crate::config_paths::absolute(&file)? == destination {
                bail!(
                    "Destination credentials alias a mcporter source store; choose --config or --scope with an independent destination directory"
                );
            }
        }
        if source.snapshot.get("tokens").is_none() {
            return Ok(Decision::Missing);
        }
        store::validate_payload(authentication, &source.snapshot)?;
        if let Some(discovery) = source
            .snapshot
            .get("discoveryState")
            .filter(|value| !value.is_null())
        {
            protocol::validate_discovery(authentication, discovery)?;
        }
        let mut incoming = store::import_content(&source.snapshot);
        incoming.insert("state".to_owned(), Value::Null);
        incoming.insert("codeVerifier".to_owned(), Value::Null);
        let destination = store::ImportDestination::inspect(authentication)?;
        if !destination.is_empty() {
            if destination.equivalent(&incoming) {
                return Ok(Decision::Unchanged);
            }
            if !replace {
                bail!(
                    "OAuth credentials for '{}' already exist; use --replace to overwrite them. No changes made.",
                    authentication.server_name
                );
            }
        }
        Ok(Decision::Import(Box::new(Self {
            destination,
            payload: Value::Object(incoming),
        })))
    }

    pub(crate) fn ensure_writable(&self) -> Result<()> {
        self.destination.ensure_writable()
    }

    pub(crate) fn commit(&self) -> Result<()> {
        self.destination.commit(&self.payload)
    }
}
