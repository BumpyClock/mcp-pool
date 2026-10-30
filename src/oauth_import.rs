use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use super::{HttpAuth, ensure_writable, files, protocol, store, string};

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
    authentication: HttpAuth,
    payload: Value,
    expected: DestinationSlot,
}

#[derive(PartialEq)]
struct DestinationSlot {
    entries: Map<String, Value>,
    server_url: Option<Value>,
}

impl DestinationSlot {
    fn inspect(authentication: &HttpAuth, vault: &Value) -> Result<Self> {
        let alias = format!("{}-oauth", authentication.server_name);
        let entries = vault
            .get("entries")
            .and_then(Value::as_object)
            .context("Invalid vault entries")?
            .iter()
            .filter(|(key, entry)| {
                *key == &authentication.identity_key
                    || string(entry, "serverName") == Some(authentication.server_name.as_str())
                    || (string(entry, "serverName") == Some(alias.as_str())
                        && string(entry, "serverUrl") == Some(authentication.server_url.as_str()))
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Ok(Self {
            entries,
            server_url: vault
                .get("serverUrls")
                .and_then(|urls| urls.get(&authentication.server_name))
                .cloned(),
        })
    }
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
        let mut incoming = content(&source.snapshot);
        incoming.insert("state".to_owned(), Value::Null);
        incoming.insert("codeVerifier".to_owned(), Value::Null);
        let vault = store::vault(authentication)?;
        let expected = DestinationSlot::inspect(authentication, &vault)?;
        if !expected.entries.is_empty() {
            let reachable = store::matching(authentication, &vault);
            if reachable.len() == expected.entries.len()
                && reachable.iter().all(|(_, entry)| {
                    content(entry) == incoming && super::reconcile::validate_client(entry).is_ok()
                })
            {
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
            authentication: authentication.clone(),
            payload: Value::Object(incoming),
            expected,
        })))
    }

    pub(crate) fn ensure_writable(&self) -> Result<()> {
        ensure_writable(&self.authentication)
    }

    pub(crate) fn commit(&self) -> Result<()> {
        self.ensure_writable()?;
        let _transaction = store::transaction(&self.authentication)?;
        let _vault_lock = files::Lock::acquire(&self.authentication.vault_path)?;
        let mut vault = store::vault(&self.authentication)?;
        let current = DestinationSlot::inspect(&self.authentication, &vault)?;
        if current != self.expected {
            bail!(
                "Destination authentication state changed during import; rerun without overwriting newer credentials"
            );
        }
        let candidates: Vec<Value> = current.entries.values().cloned().collect();
        let previous = current
            .entries
            .get(&self.authentication.identity_key)
            .or_else(|| current.entries.values().next())
            .cloned()
            .unwrap_or_default();
        // Replacement may retire old bindings, but never makes them usable under the incoming client.
        let entries = vault
            .get_mut("entries")
            .and_then(Value::as_object_mut)
            .context("Invalid vault entries")?;
        for key in current.entries.keys() {
            entries.remove(key);
        }
        store::save_entry(
            &self.authentication,
            &mut vault,
            &self.payload,
            &previous,
            &candidates,
        )
    }
}

fn content(snapshot: &Value) -> Map<String, Value> {
    let mut fields: Map<String, Value> = store::DIRECTORY_FIELDS
        .iter()
        .map(|(field, _, _)| {
            (
                (*field).to_owned(),
                snapshot.get(*field).cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    if let Some(tokens) = fields.get_mut("tokens").and_then(Value::as_object_mut) {
        tokens.remove("__mcp_pool_commit");
    }
    fields
}
