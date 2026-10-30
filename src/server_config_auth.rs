use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use super::settings::{ConfiguredEnvironment, OAuthSettings};
use super::{
    ConfiguredServer, URL_FIELDS, absolute_source, normalize_document, set_credential_scope, values,
};

/// Resolves only authentication inputs, leaving unrelated environment and header secrets untouched.
pub(crate) fn authentication_server(
    source: &Path,
    name: &str,
    entry: &Value,
) -> Result<ConfiguredServer> {
    let environment = ConfiguredEnvironment::from_raw(entry);
    let settings = OAuthSettings::from_raw(entry);
    let endpoint = URL_FIELDS
        .iter()
        .find_map(|field| entry.get(*field).and_then(Value::as_str))
        .context("Remote authentication needs a server URL")?;
    let mut projection = Map::new();
    projection.insert("baseUrl".to_owned(), json!(endpoint));
    for (field, value) in [
        ("auth", settings.auth),
        ("clientName", settings.client_name),
        ("oauthClientId", settings.client_id),
        ("oauthClientSecretEnv", settings.client_secret_env),
        ("oauthTokenEndpointAuthMethod", settings.token_auth_method),
        ("oauthRedirectUrl", settings.redirect_url),
        ("oauthClientMetadataUrl", settings.client_metadata_url),
        ("oauthRequestedScope", settings.scope),
    ] {
        if let Some(value) = value {
            projection.insert(field.to_owned(), json!(environment.expand(value)?));
        }
    }
    if let Some(directory) = settings.token_cache_directory {
        projection.insert(
            "tokenCacheDir".to_owned(),
            json!(environment.expand(directory)?),
        );
    }
    if let Some(read_only) = environment.get("MCP_POOL_CREDENTIALS_READ_ONLY")? {
        projection.insert(
            "env".to_owned(),
            json!({"MCP_POOL_CREDENTIALS_READ_ONLY":read_only}),
        );
    }
    let mut configuration = normalize_document(
        absolute_source(source)?,
        &json!({"mcpServers":{name:projection}}),
        &|name| environment.get(name),
        &values::caller_environment,
    )?;
    set_credential_scope(
        &mut configuration,
        source.parent().context("Config has no parent")?,
    );
    configuration
        .servers
        .remove(name)
        .context("Missing authentication server")
}
