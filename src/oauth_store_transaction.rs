use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::Result;
use serde_json::Value;

use super::super::{HttpAuth, files};

pub(in crate::oauth) struct Transaction {
    authentication: HttpAuth,
    _locks: Vec<files::Lock>,
}

impl Transaction {
    pub(in crate::oauth) fn save(&self, patch: &Value, expected: Option<&Value>) -> Result<()> {
        super::save_locked(&self.authentication, patch, expected)
    }

    pub(in crate::oauth) fn clear(&self) -> Result<()> {
        super::clear_locked(&self.authentication)
    }
}

pub(in crate::oauth) fn transaction(authentication: &HttpAuth) -> Result<Transaction> {
    Ok(Transaction {
        authentication: authentication.clone(),
        _locks: acquire_locks(authentication)?,
    })
}

pub(in crate::oauth) fn save(
    authentication: &HttpAuth,
    patch: &Value,
    expected: Option<&Value>,
) -> Result<()> {
    transaction(authentication)?.save(patch, expected)
}

pub(in crate::oauth) fn clear(authentication: &HttpAuth) -> Result<()> {
    transaction(authentication)?.clear()
}

/// Retains transaction ownership through blocking writes, including caller cancellation.
pub(in crate::oauth) struct AsyncTransaction(Option<Arc<Transaction>>);

impl AsyncTransaction {
    pub(in crate::oauth) async fn save(&self, patch: Value, expected: Option<Value>) -> Result<()> {
        let transaction = self
            .0
            .clone()
            .ok_or_else(|| anyhow::anyhow!("OAuth transaction released"))?;
        tokio::task::spawn_blocking(move || transaction.save(&patch, expected.as_ref())).await?
    }

    pub(in crate::oauth) async fn clear(&self) -> Result<()> {
        let transaction = self
            .0
            .clone()
            .ok_or_else(|| anyhow::anyhow!("OAuth transaction released"))?;
        tokio::task::spawn_blocking(move || transaction.clear()).await?
    }

    pub(in crate::oauth) async fn release(mut self) -> Result<()> {
        let transaction = self.0.take();
        tokio::task::spawn_blocking(move || drop(transaction)).await?;
        Ok(())
    }
}

impl Drop for AsyncTransaction {
    fn drop(&mut self) {
        if let Some(transaction) = self.0.take() {
            tokio::task::spawn_blocking(move || drop(transaction));
        }
    }
}

pub(in crate::oauth) async fn transaction_async(
    authentication: &HttpAuth,
) -> Result<AsyncTransaction> {
    let authentication = authentication.clone();
    tokio::task::spawn_blocking(move || {
        transaction(&authentication)
            .map(|transaction| AsyncTransaction(Some(Arc::new(transaction))))
    })
    .await?
}

fn filename_safe_label(label: &str) -> String {
    let mut safe = String::new();
    let mut unsafe_run = false;
    for character in label.chars() {
        if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
            safe.push(character);
            unsafe_run = false;
        } else if !unsafe_run {
            safe.push('_');
            unsafe_run = true;
        }
    }
    safe.trim_start_matches(['.', '_', '-'])
        .chars()
        .take(40)
        .collect()
}

/// Shared directory caches need one refresh lock even when their vaults differ.
fn acquire_locks(authentication: &HttpAuth) -> Result<Vec<files::Lock>> {
    let vault = super::vault(authentication)?;
    let mut identities = vec![(
        authentication
            .identity_key
            .split('|')
            .next()
            .unwrap_or_default()
            .to_owned(),
        format!("vault:{}", authentication.identity_key),
    )];
    for (key, _) in super::matching(authentication, &vault) {
        let name = key.split('|').next().unwrap_or_default().to_owned();
        identities.push((name, format!("vault:{key}")));
    }
    let root = authentication
        .vault_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("vault has no parent"))?
        .join("refresh-locks");
    let mut paths = BTreeSet::new();
    if authentication.directory_primary {
        for directory in &authentication.directory_stores {
            paths.insert(files::canonical(&directory.join(".mcp-refresh"))?);
        }
    }
    for (label, identity) in identities {
        let label = filename_safe_label(&label);
        let name = if label.is_empty() {
            files::digest(&identity)
        } else {
            format!("{label}-{}", files::digest(&identity))
        };
        paths.insert(root.join(name));
    }
    paths
        .into_iter()
        .map(|path| files::Lock::acquire(&path))
        .collect()
}
