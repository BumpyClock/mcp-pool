use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{Map, Value, json};

use super::settings::{ConfiguredEnvironment, OAuthSettings};
use super::{ConfiguredServer, absolute_source, entry::selected_url, normalize_document, values};

/// Resolves only authentication inputs, leaving unrelated environment and header secrets untouched.
pub(crate) fn authentication_server(
    source: &Path,
    name: &str,
    entry: &Value,
) -> Result<ConfiguredServer> {
    let environment = ConfiguredEnvironment::from_raw(entry);
    let settings = OAuthSettings::from_raw(entry);
    let endpoint = selected_url(entry).context("Remote authentication needs a server URL")?;
    let mut projection = Map::new();
    projection.insert("baseUrl".to_owned(), json!(endpoint));
    for (field, value) in settings.projection() {
        if let Some(value) = value {
            projection.insert(field.to_owned(), json!(environment.expand(value)?));
        }
    }
    let mut auth_environment = Map::new();
    for name in crate::environment::READ_ONLY_NAMES {
        if let Some(read_only) = environment.get(name)? {
            auth_environment.insert(name.to_owned(), json!(read_only));
        }
    }
    if !auth_environment.is_empty() {
        projection.insert("env".to_owned(), Value::Object(auth_environment));
    }
    let mut configuration = normalize_document(
        absolute_source(source)?,
        &json!({"mcpServers":{name:projection}}),
        &|name| environment.get(name),
        &values::caller_environment,
    )?;
    configuration
        .servers
        .remove(name)
        .context("Missing authentication server")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authentication_projection_preserves_both_read_only_names() -> Result<()> {
        let entry = json!({
            "baseUrl": "https://synthetic.invalid/mcp",
            "env": {
                "MCP_CLI_CREDENTIALS_READ_ONLY": "0",
                "MCP_POOL_CREDENTIALS_READ_ONLY": "1"
            }
        });
        let source = std::env::current_dir()?.join("synthetic-config.json");
        let server = authentication_server(&source, "synthetic", &entry)?;
        assert_eq!(
            server
                .definition
                .env
                .get("MCP_CLI_CREDENTIALS_READ_ONLY")
                .map(String::as_str),
            Some("0")
        );
        assert_eq!(
            server
                .definition
                .env
                .get("MCP_POOL_CREDENTIALS_READ_ONLY")
                .map(String::as_str),
            Some("1")
        );
        Ok(())
    }

    #[test]
    fn authentication_projects_aliases_without_resolving_unrelated_fields() -> Result<()> {
        let entry = json!({
            "base_url":"https://example.invalid/mcp",
            "url":"invalid-ignored-url",
            "command":"${UNRELATED_MISSING}",
            "headers":{"Authorization":"${UNRELATED_MISSING}"},
            "env":{"UNRELATED":"${UNRELATED_MISSING}","CLIENT":"configured-client"},
            "auth":"oauth",
            "oauth_client_id":"${CLIENT}",
            "oauth_requested_scope":"read",
            "oauth_client_metadata_url":"https://client.invalid/metadata",
            "token_cache_dir":"cache"
        });
        let source = std::env::current_dir()?.join("synthetic-config.json");
        let server = authentication_server(&source, "test", &entry)?;
        assert_eq!(server.definition.url, "https://example.invalid/mcp");
        assert!(server.definition.headers.is_empty());
        assert_eq!(server.oauth_settings().client_id, Some("configured-client"));
        assert_eq!(server.oauth_settings().scope, Some("read"));
        assert_eq!(
            server.oauth_settings().client_metadata_url,
            Some("https://client.invalid/metadata")
        );
        assert_eq!(server.oauth_settings().token_cache_directory, Some("cache"));
        assert!(server.raw.get("command").is_none());
        assert!(server.raw.pointer("/env/UNRELATED").is_none());
        Ok(())
    }
}
