use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::support::Fixture;

pub fn credentials(name: &str, url: &str, access: &str) -> Value {
    json!({
        "serverName":name,"serverUrl":url,"updatedAt":"2026-01-01T00:00:00.000Z",
        "tokens":{"access_token":access,"token_type":"Bearer","refresh_token":"synthetic-refresh","expires_at":4_000_000_000_u64},
        "clientInfo":{"client_id":"synthetic-client","token_endpoint_auth_method":"none"},
        "state":{"state":"unfinished-consent"},"codeVerifier":"unfinished-verifier"
    })
}

pub fn vault(name: &str, url: &str, entry: Value) -> Value {
    let descriptor = format!(r#"{{"name":"{name}","url":"{url}","command":null}}"#);
    let digest = format!("{:x}", Sha256::digest(descriptor.as_bytes()));
    let key = format!("{name}|{}", digest.get(..16).unwrap_or_default());
    json!({"version":2,"entries":{key:entry},"serverUrls":{name:url}})
}

pub fn destination(fixture: &Fixture) -> Result<std::path::PathBuf> {
    Ok(fixture
        .home_config()
        .parent()
        .context("config parent")?
        .join("credentials.json"))
}

pub fn only_entry(document: &Value) -> Result<&Value> {
    let entries = document
        .get("entries")
        .and_then(Value::as_object)
        .context("vault entries")?;
    ensure!(entries.len() == 1, "expected one credential entry");
    entries.values().next().context("credential entry")
}
