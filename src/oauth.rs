use std::fmt;
use std::path::PathBuf;

use anyhow::{Result, bail};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::ServerDef;
use crate::server_config::ConfiguredServer;

#[cfg(test)]
#[path = "oauth_authorization_tests.rs"]
mod authorization_tests;
#[path = "oauth_browser.rs"]
mod browser;
#[path = "oauth_files.rs"]
mod files;
#[path = "oauth_flow.rs"]
mod flow;
#[path = "oauth_protocol.rs"]
mod protocol;
#[path = "oauth_reconcile.rs"]
mod reconcile;
#[path = "oauth_stdio.rs"]
mod stdio;
#[path = "oauth_store.rs"]
mod store;
#[cfg(test)]
#[path = "oauth_store_tests.rs"]
mod store_tests;
#[cfg(test)]
#[path = "oauth_test_support.rs"]
mod test_support;
#[cfg(test)]
#[path = "oauth_tests.rs"]
mod tests;

/// Persisted transport context deliberately excludes tokens and client secrets.
#[derive(Clone, Serialize, Deserialize)]
pub struct HttpAuth {
    pub server_name: String,
    pub server_url: String,
    pub vault_path: PathBuf,
    pub directory_stores: Vec<PathBuf>,
    pub directory_primary: bool,
    pub identity_key: String,
    pub client_name: String,
    pub client_id: Option<String>,
    pub client_secret_env: Option<String>,
    pub token_auth_method: Option<String>,
    pub redirect_url: Option<String>,
    pub client_metadata_url: Option<String>,
    pub scope: Option<String>,
    pub timeout_ms: u64,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub cached_only: bool,
}

impl fmt::Debug for HttpAuth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("HttpAuth { identity: [redacted], storage: [redacted] }")
    }
}

pub struct AuthOptions {
    pub reset: bool,
    pub no_browser: bool,
    pub json: bool,
}

pub async fn prepare(server: &ConfiguredServer, no_oauth: bool) -> Result<ServerDef> {
    let mut definition = server.definition.clone();
    if !definition.is_remote() {
        definition.auth = None;
        return Ok(definition);
    }
    let mut authentication = context(server)?;
    authentication.cached_only = no_oauth;
    let snapshot = read_snapshot(&authentication).await?;
    let configured = setting(&server.raw, "auth", "auth")
        .is_some_and(|method| method.eq_ignore_ascii_case("oauth"));
    if configured || snapshot.get("tokens").is_some() {
        store::validate_binding(&authentication, &snapshot)?;
        let tokens = snapshot
            .get("tokens")
            .ok_or_else(|| missing(&authentication))?;
        store::validate_tokens(tokens)?;
        if store::expired(tokens) {
            if authentication.read_only {
                ensure_writable(&authentication)?;
            }
            if no_oauth || string(tokens, "refresh_token").is_none() {
                return Err(missing(&authentication));
            }
        }
        definition.auth = Some(authentication);
    }
    Ok(definition)
}

pub async fn authorization_header(authentication: &HttpAuth) -> Result<Option<String>> {
    let snapshot = read_snapshot(authentication).await?;
    store::validate_binding(authentication, &snapshot)?;
    let tokens = snapshot
        .get("tokens")
        .ok_or_else(|| missing(authentication))?;
    store::validate_tokens(tokens)?;
    let access_token = if store::expired(tokens) {
        protocol::refresh(authentication).await?
    } else {
        string(tokens, "access_token")
            .ok_or_else(|| missing(authentication))?
            .to_owned()
    };
    Ok(Some(format!("Bearer {access_token}")))
}

#[cfg(test)]
pub async fn authorize(server: &ConfiguredServer, options: AuthOptions) -> Result<Value> {
    authorize_with_timeout(server, options, None).await
}

pub async fn authorize_with_timeout(
    server: &ConfiguredServer,
    options: AuthOptions,
    timeout_ms: Option<u64>,
) -> Result<Value> {
    if timeout_ms == Some(0) {
        bail!("OAuth timeout must be greater than zero milliseconds");
    }
    if timeout_ms.is_some_and(|milliseconds| {
        tokio::time::Instant::now()
            .checked_add(std::time::Duration::from_millis(milliseconds))
            .is_none()
    }) {
        bail!("OAuth timeout exceeds the supported clock range");
    }
    if !server.definition.is_remote() {
        return stdio::authorize(server, options, timeout_ms).await;
    }
    flow::authorize(server, options, timeout_ms).await
}

pub fn vault_set(server: &ConfiguredServer, payload: &Value) -> Result<()> {
    let authentication = context(server)?;
    ensure_writable(&authentication)?;
    let mut payload = if payload.get("access_token").is_some() {
        json!({"tokens": payload})
    } else {
        payload.clone()
    };
    let tokens = payload
        .get("tokens")
        .ok_or_else(|| anyhow::anyhow!("vault payload requires tokens"))?;
    store::validate_tokens(tokens)?;
    if string(tokens, "refresh_token").is_some()
        && payload
            .get("clientInfo")
            .and_then(|client| string(client, "client_id"))
            .is_none()
    {
        bail!("refreshable credential payload must include its bound clientInfo registration");
    }
    store::validate_binding(&authentication, &payload)?;
    if let Some(tokens) = payload.get_mut("tokens").and_then(Value::as_object_mut) {
        if !tokens.contains_key("expires_at")
            && !tokens.contains_key("expiresAt")
            && let Some(duration) = tokens.get("expires_in").and_then(Value::as_f64)
        {
            tokens.insert(
                "expires_at".to_owned(),
                json!(files::now().saturating_add(duration as u64)),
            );
        }
        tokens.insert(
            "__mcporter_generation".to_owned(),
            json!(oauth2::CsrfToken::new_random().secret()),
        );
    }
    let _transaction = store::transaction(&authentication)?;
    store::save(&authentication, &payload, None)
}

pub fn vault_clear(server: &ConfiguredServer) -> Result<()> {
    let authentication = context(server)?;
    ensure_writable(&authentication)?;
    let _transaction = store::transaction(&authentication)?;
    store::clear(&authentication)
}

pub fn credential_status(server: &ConfiguredServer) -> Result<Value> {
    let authentication = context(server)?;
    let snapshot = store::read(&authentication)?;
    let binding_valid = store::validate_binding(&authentication, &snapshot).is_ok();
    let tokens = snapshot.get("tokens");
    let valid = tokens.is_some_and(|value| store::validate_tokens(value).is_ok());
    Ok(json!({
        "server": server.name,
        "authenticated": binding_valid && valid && tokens.is_some_and(|value| !store::expired(value)),
        "hasTokens": valid,
        "hasClientRegistration": snapshot.get("clientInfo").and_then(|client| string(client, "client_id")).is_some(),
        "refreshable": valid && tokens.and_then(|value| string(value, "refresh_token")).is_some(),
        "expired": tokens.map(store::expired),
        "bindingValid": binding_valid,
    }))
}

pub(crate) fn context(server: &ConfiguredServer) -> Result<HttpAuth> {
    let definition = &server.definition;
    let home = definition
        .env
        .get("HOME")
        .or_else(|| definition.env.get("USERPROFILE"))
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(PathBuf::from)
        })
        .or_else(dirs::home_dir)
        .ok_or_else(|| anyhow::anyhow!("home directory unavailable for credentials"))?;
    let data = definition
        .env
        .get("XDG_DATA_HOME")
        .cloned()
        .or_else(|| std::env::var("XDG_DATA_HOME").ok())
        .map(PathBuf::from)
        .filter(|path| path.is_absolute());
    let vault_root = data
        .map(|path| path.join("mcporter"))
        .unwrap_or_else(|| home.join(".mcporter"));
    let server_url = if definition.is_remote() {
        Url::parse(&definition.url)
            .map_err(|_| anyhow::anyhow!("invalid MCP server URL"))?
            .to_string()
    } else {
        String::new()
    };
    #[derive(Serialize)]
    struct CommandIdentity<'a> {
        command: &'a str,
        args: &'a [String],
    }
    #[derive(Serialize)]
    struct Identity<'a> {
        name: &'a str,
        url: Option<&'a str>,
        command: Option<CommandIdentity<'a>>,
    }
    let identity = Identity {
        name: &server.name,
        url: definition.is_remote().then_some(server_url.as_str()),
        command: (!definition.is_remote()).then_some(CommandIdentity {
            command: &definition.command,
            args: &definition.args,
        }),
    };
    let serialized = serde_json::to_string(&identity)?;
    let identity_key = format!("{}|{}", server.name, files::digest(&serialized));
    let mut directory_stores = Vec::new();
    if let Some(directory) = setting(&server.raw, "tokenCacheDir", "token_cache_dir") {
        let path = if directory == "~" {
            home
        } else if let Some(suffix) = directory
            .strip_prefix("~/")
            .or_else(|| directory.strip_prefix("~\\"))
        {
            home.join(suffix)
        } else {
            PathBuf::from(directory)
        };
        directory_stores.push(if path.is_absolute() {
            path
        } else {
            server
                .source
                .parent()
                .ok_or_else(|| anyhow::anyhow!("config path has no parent"))?
                .join(path)
        });
    } else if !server.name.contains(['/', '\\']) && server.name != "." && server.name != ".." {
        directory_stores.push(home.join(".mcporter").join(&server.name));
    }
    Ok(HttpAuth {
        server_name: server.name.clone(),
        server_url,
        vault_path: vault_root.join("credentials.json"),
        directory_stores,
        identity_key,
        directory_primary: setting(&server.raw, "tokenCacheDir", "token_cache_dir").is_some(),
        client_name: setting(&server.raw, "clientName", "client_name")
            .unwrap_or("mcp-pool")
            .to_owned(),
        client_id: setting(&server.raw, "oauthClientId", "oauth_client_id").map(str::to_owned),
        client_secret_env: setting(
            &server.raw,
            "oauthClientSecretEnv",
            "oauth_client_secret_env",
        )
        .map(str::to_owned),
        token_auth_method: setting(
            &server.raw,
            "oauthTokenEndpointAuthMethod",
            "oauth_token_endpoint_auth_method",
        )
        .map(str::to_owned),
        redirect_url: setting(&server.raw, "oauthRedirectUrl", "oauth_redirect_url")
            .map(str::to_owned),
        client_metadata_url: setting(
            &server.raw,
            "oauthClientMetadataUrl",
            "oauth_client_metadata_url",
        )
        .map(str::to_owned),
        scope: setting(&server.raw, "oauthRequestedScope", "oauth_requested_scope")
            .or_else(|| setting(&server.raw, "oauthScope", "oauth_scope"))
            .map(str::to_owned),
        timeout_ms: definition
            .timeout_ms
            .unwrap_or(300_000)
            .clamp(1_000, 900_000),
        read_only: definition
            .env
            .get("MCP_POOL_CREDENTIALS_READ_ONLY")
            .is_some_and(|value| value == "1")
            || std::env::var("MCP_POOL_CREDENTIALS_READ_ONLY").is_ok_and(|value| value == "1"),
        cached_only: false,
    })
}

pub(crate) fn setting<'a>(value: &'a Value, camel: &str, snake: &str) -> Option<&'a str> {
    string(value, camel).or_else(|| string(value, snake))
}

pub(crate) fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

pub(crate) fn missing(authentication: &HttpAuth) -> anyhow::Error {
    anyhow::anyhow!(
        "OAuth credentials missing or expired; run `mcp-pool auth {}` explicitly",
        authentication.server_name
    )
}

pub(crate) fn ensure_writable(authentication: &HttpAuth) -> Result<()> {
    if authentication.read_only
        || std::env::var("MCP_POOL_CREDENTIALS_READ_ONLY").is_ok_and(|value| value == "1")
    {
        bail!(
            "credential validation is read-only; expired tokens cannot refresh and auth/vault mutations are disabled; remove MCP_POOL_CREDENTIALS_READ_ONLY only for an explicitly authorized runtime operation"
        );
    }
    Ok(())
}

pub(crate) async fn read_snapshot(authentication: &HttpAuth) -> Result<Value> {
    let authentication = authentication.clone();
    tokio::task::spawn_blocking(move || store::read(&authentication)).await?
}

pub(crate) fn checked_url(value: &str) -> Result<Url> {
    let url = Url::parse(value).map_err(|_| anyhow::anyhow!("invalid OAuth endpoint URL"))?;
    let loopback = matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    );
    if (url.scheme() != "https" && !(url.scheme() == "http" && loopback))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        bail!(
            "OAuth endpoints require HTTPS (HTTP allowed only on loopback), without userinfo or fragments"
        );
    }
    Ok(url)
}
