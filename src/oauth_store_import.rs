use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

use super::super::{HttpAuth, ensure_writable, files, string};

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

pub(in crate::oauth) struct ImportDestination {
    authentication: HttpAuth,
    expected: DestinationSlot,
    all_reachable: bool,
}

impl ImportDestination {
    pub(in crate::oauth) fn inspect(authentication: &HttpAuth) -> Result<Self> {
        let vault = super::vault(authentication)?;
        let expected = DestinationSlot::inspect(authentication, &vault)?;
        let all_reachable = super::matching(authentication, &vault).len() == expected.entries.len();
        Ok(Self {
            authentication: authentication.clone(),
            expected,
            all_reachable,
        })
    }

    pub(in crate::oauth) fn is_empty(&self) -> bool {
        self.expected.entries.is_empty()
    }

    pub(in crate::oauth) fn equivalent(&self, incoming: &Map<String, Value>) -> bool {
        self.all_reachable
            && self.expected.entries.values().all(|entry| {
                import_content(entry) == *incoming
                    && super::super::reconcile::validate_client(entry).is_ok()
            })
    }

    pub(in crate::oauth) fn ensure_writable(&self) -> Result<()> {
        ensure_writable(&self.authentication)
    }

    pub(in crate::oauth) fn commit(&self, payload: &Value) -> Result<()> {
        self.ensure_writable()?;
        let _transaction = super::transaction(&self.authentication)?;
        let _vault_lock = files::Lock::acquire(&self.authentication.vault_path)?;
        let mut vault = super::vault(&self.authentication)?;
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
        super::save_entry(
            &self.authentication,
            &mut vault,
            payload,
            &previous,
            &candidates,
        )
    }
}

pub(in crate::oauth) fn import_content(snapshot: &Value) -> Map<String, Value> {
    let mut fields: Map<String, Value> = super::DIRECTORY_FIELDS
        .iter()
        .map(|(field, _, _)| {
            (
                (*field).to_owned(),
                snapshot.get(*field).cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    if let Some(tokens) = fields.get_mut("tokens") {
        super::super::reconcile::strip_commit_metadata(tokens);
    }
    fields
}
