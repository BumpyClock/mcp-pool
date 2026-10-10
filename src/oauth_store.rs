use std::collections::BTreeSet;
use std::fs;

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::{HttpAuth, files, string};

const DIRECTORY_FIELDS: &[(&str, &str, bool)] = &[
    ("tokens", "tokens.json", true),
    ("clientInfo", "client.json", true),
    ("discoveryState", "discovery.json", true),
    (
        "authorizationServerUrl",
        "authorization_server_url.txt",
        false,
    ),
    ("resourceUrl", "resource_url.txt", false),
    ("state", "state.txt", true),
    ("codeVerifier", "code_verifier.txt", false),
];

fn vault(authentication: &HttpAuth) -> Result<Value> {
    let value = files::read_json(&authentication.vault_path)?
        .unwrap_or_else(|| json!({"version":2,"entries":{},"serverUrls":{}}));
    if !matches!(value.get("version").and_then(Value::as_u64), Some(1 | 2))
        || !value.get("entries").is_some_and(Value::is_object)
    {
        bail!("unsupported or malformed mcporter credential vault");
    }
    Ok(value)
}

fn matching<'a>(authentication: &HttpAuth, vault: &'a Value) -> Vec<(String, &'a Value)> {
    let marker = vault
        .get("serverUrls")
        .and_then(|urls| string(urls, &authentication.server_name));
    if marker.is_some_and(|url| url != authentication.server_url) {
        return Vec::new();
    }
    let Some(entries) = vault.get("entries").and_then(Value::as_object) else {
        return Vec::new();
    };
    let alias_name = format!("{}-oauth", authentication.server_name);
    let mut found: Vec<_> = entries
        .iter()
        .filter(|(key, entry)| {
            let exact = *key == &authentication.identity_key;
            let alias = string(entry, "serverName").is_some_and(|name| name == alias_name)
                && string(entry, "serverUrl") == Some(authentication.server_url.as_str());
            (exact || alias)
                && string(entry, "serverUrl").is_none_or(|url| url == authentication.server_url)
                && string(entry, "serverName")
                    .is_some_and(|name| name == authentication.server_name || name == alias_name)
        })
        .map(|(key, entry)| (key.clone(), entry))
        .collect();
    found.sort_by(|(left_key, left), (right_key, right)| {
        (right_key == &authentication.identity_key)
            .cmp(&(left_key == &authentication.identity_key))
            .then_with(|| string(right, "updatedAt").cmp(&string(left, "updatedAt")))
    });
    found
}

fn directory(authentication: &HttpAuth, root: &std::path::Path) -> Result<Value> {
    let marker = files::read_text(&root.join("server_url.txt"))?;
    if marker
        .as_deref()
        .is_some_and(|url| url.trim() != authentication.server_url)
    {
        return Ok(json!({}));
    }
    let mut snapshot = serde_json::Map::new();
    for (field, file, structured) in DIRECTORY_FIELDS {
        let value = if *structured {
            files::read_json(&root.join(file))?
        } else {
            files::read_text(&root.join(file))?.map(|text| Value::String(text.trim().to_owned()))
        };
        if let Some(value) = value {
            snapshot.insert((*field).to_owned(), value);
        }
    }
    Ok(Value::Object(snapshot))
}

fn snapshots(authentication: &HttpAuth) -> Result<Vec<Value>> {
    let vault = vault(authentication)?;
    let matches = matching(authentication, &vault);
    let mut candidates: Vec<Value> = matches.iter().map(|(_, entry)| (*entry).clone()).collect();
    for directory_root in &authentication.directory_stores {
        let snapshot = directory(authentication, directory_root)?;
        if authentication.directory_primary {
            candidates.insert(0, snapshot);
        } else {
            candidates.push(snapshot);
        }
    }
    Ok(candidates)
}

pub(super) fn read(authentication: &HttpAuth) -> Result<Value> {
    let selected = super::reconcile::select(&snapshots(authentication)?)?;
    validate_binding(authentication, &selected)?;
    Ok(selected)
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

/// Shares lock paths with mcporter so both processes serialize refreshes of the same token.
pub(super) fn transaction(authentication: &HttpAuth) -> Result<Vec<files::Lock>> {
    let vault = vault(authentication)?;
    let mut identities = vec![(
        authentication
            .identity_key
            .split('|')
            .next()
            .unwrap_or_default()
            .to_owned(),
        format!("vault:{}", authentication.identity_key),
    )];
    for (key, _) in matching(authentication, &vault) {
        let name = key.split('|').next().unwrap_or_default().to_owned();
        identities.push((name, format!("vault:{key}")));
    }
    if authentication.directory_primary {
        for root in &authentication.directory_stores {
            let path = files::canonical(&root.join("tokens.json"))?;
            identities.push((
                path.parent()
                    .and_then(std::path::Path::file_name)
                    .map(|value| value.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                format!("dir:{}", files::portable_path(&path)),
            ));
        }
    }
    let root = authentication
        .vault_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("vault has no parent"))?
        .join("refresh-locks");
    let mut paths = BTreeSet::new();
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

pub(super) async fn transaction_async(authentication: &HttpAuth) -> Result<Vec<files::Lock>> {
    let authentication = authentication.clone();
    tokio::task::spawn_blocking(move || transaction(&authentication)).await?
}

pub(super) fn validate_tokens(tokens: &Value) -> Result<()> {
    let access = string(tokens, "access_token")
        .ok_or_else(|| anyhow::anyhow!("credential access token missing"))?;
    if !string(tokens, "token_type").is_some_and(|kind| kind.eq_ignore_ascii_case("bearer"))
        || reqwest::header::HeaderValue::from_str(&format!("Bearer {access}")).is_err()
    {
        bail!("invalid bearer credential");
    }
    for field in ["expires_in", "expires_at", "expiresAt"] {
        if tokens.get(field).is_some_and(|value| {
            !value.is_number() || value.as_f64().is_none_or(|number| number < 0.0)
        }) {
            bail!("invalid credential expiration");
        }
    }
    Ok(())
}

pub(super) fn expired(tokens: &Value) -> bool {
    let expiration = tokens
        .get("expires_at")
        .or_else(|| tokens.get("expiresAt"))
        .and_then(Value::as_f64)
        .map(|value| value as u64)
        .unwrap_or_default();
    if expiration > 0 {
        return expiration <= files::now().saturating_add(60);
    }
    tokens.get("expires_in").is_some()
}

pub(super) fn validate_binding(authentication: &HttpAuth, snapshot: &Value) -> Result<()> {
    if string(snapshot, "serverUrl").is_some_and(|url| url != authentication.server_url) {
        bail!("credential server URL binding changed; explicitly reauthorize");
    }
    if let Some(configured) = &authentication.client_id
        && snapshot
            .get("clientInfo")
            .and_then(|client| string(client, "client_id"))
            .is_some_and(|stored| stored != configured)
    {
        bail!("credential client registration binding changed; explicitly reauthorize");
    }
    let tokens_issuer = snapshot
        .get("tokens")
        .and_then(|tokens| string(tokens, "issuer"));
    let client_issuer = snapshot
        .get("clientInfo")
        .and_then(|client| string(client, "issuer"));
    let metadata_issuer = snapshot
        .get("discoveryState")
        .and_then(|state| state.get("authorizationServerMetadata"))
        .and_then(|metadata| string(metadata, "issuer"));
    let mut expected = None;
    for issuer in [
        tokens_issuer,
        client_issuer,
        metadata_issuer,
        string(snapshot, "authorizationServerUrl"),
    ]
    .into_iter()
    .flatten()
    {
        super::checked_url(issuer)?;
        let issuer = issuer.trim_end_matches('/');
        if expected.is_some_and(|previous| previous != issuer) {
            bail!("OAuth issuer binding mismatch; explicitly reauthorize");
        }
        expected = Some(issuer);
    }
    if let Some(resource) = string(snapshot, "resourceUrl") {
        validate_resource(&authentication.server_url, resource)?;
    }
    if let Some(resource) = snapshot
        .get("discoveryState")
        .and_then(|state| state.get("resourceMetadata"))
        .and_then(|metadata| string(metadata, "resource"))
    {
        validate_resource(&authentication.server_url, resource)?;
        if string(snapshot, "resourceUrl").is_some_and(|stored| stored != resource) {
            bail!("OAuth resource metadata binding mismatch");
        }
    }
    Ok(())
}

pub(super) fn validate_resource(server: &str, resource: &str) -> Result<()> {
    let server = super::checked_url(server)?;
    let resource = super::checked_url(resource)?;
    let prefix = resource.path().trim_end_matches('/');
    if server.origin() != resource.origin()
        || resource.query().is_some()
        || !(server.path() == prefix
            || prefix.is_empty()
            || server.path().starts_with(&format!("{prefix}/")))
    {
        bail!("protected resource does not match configured MCP endpoint");
    }
    Ok(())
}

/// Writes the vault first so reconciliation can recover if a secondary store fails.
pub(super) fn save(
    authentication: &HttpAuth,
    patch: &Value,
    expected: Option<&Value>,
) -> Result<()> {
    super::ensure_writable(authentication)?;
    if let Some(expected) = expected {
        let latest = read(authentication)?;
        if latest.get("tokens") != expected.get("tokens")
            || latest.get("clientInfo") != expected.get("clientInfo")
        {
            bail!(
                "credentials changed during authorization; retry without resetting fresh credentials"
            );
        }
    }
    let _vault_lock = files::Lock::acquire(&authentication.vault_path)?;
    let mut vault = vault(authentication)?;
    let candidates = snapshots(authentication)?;
    let previous = super::reconcile::select(&candidates)?;
    let mut entry = previous.as_object().cloned().unwrap_or_default();
    let patch = patch
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("credential payload must be an object"))?;
    for (key, value) in patch {
        if ![
            "tokens",
            "clientInfo",
            "discoveryState",
            "authorizationServerUrl",
            "resourceUrl",
            "state",
            "codeVerifier",
        ]
        .contains(&key.as_str())
        {
            bail!("unsupported credential payload field");
        }
        entry.insert(key.clone(), value.clone());
    }
    entry.insert("serverName".to_owned(), json!(authentication.server_name));
    entry.insert("serverUrl".to_owned(), json!(authentication.server_url));
    entry.insert("updatedAt".to_owned(), json!(files::timestamp()));
    let mut entry = Value::Object(entry);
    if patch.contains_key("tokens") {
        super::reconcile::stamp(&mut entry, &previous, &candidates)?;
    }
    super::reconcile::validate_client(&entry)?;
    validate_binding(authentication, &entry)?;
    let entries = vault
        .get_mut("entries")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("invalid vault entries"))?;
    entries.retain(|_, value| {
        !(string(value, "serverName") == Some(authentication.server_name.as_str())
            && string(value, "serverUrl").is_some_and(|url| url != authentication.server_url))
    });
    entries.insert(authentication.identity_key.clone(), entry.clone());
    if let Some(root) = vault.as_object_mut() {
        root.insert("version".to_owned(), json!(2));
        let urls = root
            .entry("serverUrls")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("invalid vault URL bindings"))?;
        urls.insert(
            authentication.server_name.clone(),
            json!(authentication.server_url),
        );
    }
    files::write_json(&authentication.vault_path, &vault)?;
    if authentication.directory_primary {
        for directory in &authentication.directory_stores {
            let _directory_lock = files::Lock::acquire(&directory.join("tokens.json"))?;
            for (field, file, structured) in DIRECTORY_FIELDS {
                if let Some(value) = entry.get(*field) {
                    if value.is_null() {
                        match fs::remove_file(directory.join(file)) {
                            Ok(()) => {}
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                            Err(_) => bail!("could not retire completed OAuth transaction state"),
                        }
                    } else if *structured {
                        files::write_json(&directory.join(file), value)?;
                    } else if let Some(text) = value.as_str() {
                        files::atomic_write(&directory.join(file), text.as_bytes())?;
                    }
                }
            }
            files::atomic_write(
                &directory.join("server_url.txt"),
                authentication.server_url.as_bytes(),
            )?;
        }
    }
    Ok(())
}

pub(super) async fn save_async(
    authentication: &HttpAuth,
    patch: Value,
    expected: Option<Value>,
) -> Result<()> {
    let authentication = authentication.clone();
    tokio::task::spawn_blocking(move || save(&authentication, &patch, expected.as_ref())).await?
}

pub(super) fn clear(authentication: &HttpAuth) -> Result<()> {
    super::ensure_writable(authentication)?;
    let _vault_lock = files::Lock::acquire(&authentication.vault_path)?;
    let mut vault = vault(authentication)?;
    let keys: Vec<_> = matching(authentication, &vault)
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    if let Some(entries) = vault.get_mut("entries").and_then(Value::as_object_mut) {
        for key in keys {
            entries.remove(&key);
        }
    }
    files::write_json(&authentication.vault_path, &vault)?;
    for directory in &authentication.directory_stores {
        if !directory.exists() {
            continue;
        }
        let _directory_lock = files::Lock::acquire(&directory.join("tokens.json"))?;
        for (_, file, _) in DIRECTORY_FIELDS {
            match fs::remove_file(directory.join(file)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => bail!("could not clear selected directory credential"),
            }
        }
    }
    Ok(())
}
