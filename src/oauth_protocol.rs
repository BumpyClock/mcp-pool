use std::time::Duration;

use anyhow::{Result, bail};
use oauth2::{
    AuthType, AuthUrl, ClientId, ClientSecret, EndpointNotSet, EndpointSet, RefreshToken,
    TokenResponse, TokenUrl,
};
use reqwest::{Client, Url};
use serde_json::{Value, json};

use super::{HttpAuth, checked_url, files, missing, read_snapshot, store, string};

pub(super) type OAuthClient = oauth2::basic::BasicClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointSet,
>;

pub(super) fn http() -> Result<Client> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| anyhow::anyhow!("could not initialize OAuth HTTP client"))
}

pub(super) async fn json_response(mut response: reqwest::Response) -> Result<Value> {
    if !response.status().is_success() {
        bail!(
            "OAuth endpoint rejected request (HTTP {}); credentials were not cleared",
            response.status().as_u16()
        );
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("OAuth response could not be read"))?
    {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            bail!("OAuth response exceeds size limit");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("OAuth endpoint returned malformed JSON"))
}

async fn optional_json(client: &Client, url: Url) -> Result<Option<Value>> {
    let response = client
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("OAuth metadata request failed"))?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    Ok(Some(json_response(response).await?))
}

fn well_known(base: &Url, kind: &str, insert_path: bool) -> Url {
    let mut url = base.clone();
    let suffix = if insert_path {
        base.path().trim_end_matches('/')
    } else {
        ""
    };
    url.set_path(&format!("/.well-known/{kind}{suffix}"));
    url.set_query(None);
    url.set_fragment(None);
    url
}

/// Existing credentials stay bound to their issuer; changed issuers require explicit reauthorization.
pub(super) async fn discover(
    authentication: &HttpAuth,
    snapshot: &Value,
    fresh: bool,
) -> Result<Value> {
    if !fresh
        && let Some(state) = snapshot.get("discoveryState")
        && state.get("authorizationServerMetadata").is_some()
    {
        validate_discovery(authentication, state)?;
        return Ok(state.clone());
    }
    let client = http()?;
    let server = checked_url(&authentication.server_url)?;
    let mut resource_metadata_url = well_known(&server, "oauth-protected-resource", true);
    let mut resource_metadata = optional_json(&client, resource_metadata_url.clone()).await?;
    if resource_metadata.is_none() && server.path() != "/" {
        resource_metadata_url = well_known(&server, "oauth-protected-resource", false);
        resource_metadata = optional_json(&client, resource_metadata_url.clone()).await?;
    }
    if resource_metadata.is_none() {
        let response = client
            .get(server.clone())
            .header("Accept", "application/json, text/event-stream")
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("MCP OAuth challenge request failed"))?;
        if let Some(challenge) = response
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            && let Some(value) = challenge
                .split("resource_metadata=\"")
                .nth(1)
                .and_then(|value| value.split('"').next())
        {
            resource_metadata_url = checked_url(value)?;
            resource_metadata = optional_json(&client, resource_metadata_url.clone()).await?;
        }
    }
    let issuer = if let Some(resource) = &resource_metadata {
        let configured_resource = string(resource, "resource")
            .ok_or_else(|| anyhow::anyhow!("resource metadata missing resource"))?;
        store::validate_resource(&authentication.server_url, configured_resource)?;
        let servers = resource
            .get("authorization_servers")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("resource metadata has no authorization servers"))?;
        let bound = string(snapshot, "authorizationServerUrl").or_else(|| {
            snapshot
                .get("tokens")
                .and_then(|tokens| string(tokens, "issuer"))
        });
        let chosen = if let Some(bound) = bound {
            servers
                .iter()
                .filter_map(Value::as_str)
                .find(|value| value.trim_end_matches('/') == bound.trim_end_matches('/'))
                .ok_or_else(|| {
                    anyhow::anyhow!("OAuth issuer changed; reset and explicitly reauthorize")
                })?
        } else {
            servers
                .first()
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("authorization server missing"))?
        };
        checked_url(chosen)?
    } else {
        let mut origin = server.clone();
        origin.set_path("/");
        origin.set_query(None);
        origin
    };
    if issuer.query().is_some() {
        bail!("OAuth issuer URL must not contain a query");
    }
    let mut metadata = optional_json(
        &client,
        well_known(&issuer, "oauth-authorization-server", true),
    )
    .await?;
    if metadata.is_none() {
        let mut oidc = issuer.clone();
        oidc.set_path(&format!(
            "{}/.well-known/openid-configuration",
            issuer.path().trim_end_matches('/')
        ));
        metadata = optional_json(&client, oidc).await?;
    }
    let metadata = metadata.ok_or_else(|| {
        anyhow::anyhow!("OAuth server metadata unavailable; configure an actual supported provider")
    })?;
    let received = string(&metadata, "issuer")
        .ok_or_else(|| anyhow::anyhow!("OAuth metadata missing issuer"))?;
    if received.trim_end_matches('/') != issuer.as_str().trim_end_matches('/') {
        bail!("OAuth metadata issuer mismatch");
    }
    let state = json!({
        "authorizationServerUrl": issuer.as_str(),
        "resourceMetadataUrl": resource_metadata.as_ref().map(|_| resource_metadata_url.as_str()),
        "resourceMetadata": resource_metadata,
        "authorizationServerMetadata": metadata,
    });
    validate_discovery(authentication, &state)?;
    Ok(state)
}

pub(super) fn validate_discovery(authentication: &HttpAuth, state: &Value) -> Result<()> {
    let metadata = state
        .get("authorizationServerMetadata")
        .ok_or_else(|| anyhow::anyhow!("OAuth server discovery is missing"))?;
    let issuer = string(metadata, "issuer")
        .ok_or_else(|| anyhow::anyhow!("OAuth metadata missing issuer"))?;
    let issuer_url = checked_url(issuer)?;
    if issuer_url.query().is_some() {
        bail!("OAuth issuer contains a query");
    }
    for endpoint in ["authorization_endpoint", "token_endpoint"] {
        checked_url(
            string(metadata, endpoint)
                .ok_or_else(|| anyhow::anyhow!("OAuth metadata missing endpoint"))?,
        )?;
    }
    if string(state, "authorizationServerUrl")
        .is_some_and(|stored| stored.trim_end_matches('/') != issuer.trim_end_matches('/'))
    {
        bail!("OAuth discovery issuer mismatch");
    }
    if let Some(resource) = state
        .get("resourceMetadata")
        .and_then(|metadata| string(metadata, "resource"))
    {
        store::validate_resource(&authentication.server_url, resource)?;
    }
    if let Some(servers) = state
        .get("resourceMetadata")
        .and_then(|metadata| metadata.get("authorization_servers"))
        .and_then(Value::as_array)
        && !servers
            .iter()
            .filter_map(Value::as_str)
            .any(|server| server.trim_end_matches('/') == issuer.trim_end_matches('/'))
    {
        bail!("discovered issuer is not authorized for configured protected resource");
    }
    Ok(())
}

pub(super) fn client(
    authentication: &HttpAuth,
    info: &Value,
    discovery: &Value,
) -> Result<OAuthClient> {
    let metadata = discovery
        .get("authorizationServerMetadata")
        .ok_or_else(|| anyhow::anyhow!("OAuth metadata missing"))?;
    let identifier = string(info, "client_id")
        .ok_or_else(|| anyhow::anyhow!("OAuth client registration missing"))?;
    let mut client = oauth2::basic::BasicClient::new(ClientId::new(identifier.to_owned()))
        .set_auth_uri(
            AuthUrl::new(
                string(metadata, "authorization_endpoint")
                    .ok_or_else(|| anyhow::anyhow!("authorization endpoint missing"))?
                    .to_owned(),
            )
            .map_err(|_| anyhow::anyhow!("invalid authorization endpoint"))?,
        )
        .set_token_uri(
            TokenUrl::new(
                string(metadata, "token_endpoint")
                    .ok_or_else(|| anyhow::anyhow!("token endpoint missing"))?
                    .to_owned(),
            )
            .map_err(|_| anyhow::anyhow!("invalid token endpoint"))?,
        );
    let secret = if let Some(secret) = string(info, "client_secret") {
        Some(secret.to_owned())
    } else if let Some(variable) = &authentication.client_secret_env {
        Some(std::env::var(variable).map_err(|_| {
            anyhow::anyhow!("configured OAuth client-secret environment variable missing")
        })?)
    } else {
        None
    };
    let method = authentication
        .token_auth_method
        .as_deref()
        .or_else(|| string(info, "token_endpoint_auth_method"))
        .unwrap_or(if secret.is_some() {
            "client_secret_basic"
        } else {
            "none"
        });
    if let Some(methods) = metadata
        .get("token_endpoint_auth_methods_supported")
        .and_then(Value::as_array)
        && !methods
            .iter()
            .any(|supported| supported.as_str() == Some(method))
    {
        bail!("configured OAuth client authentication method is unsupported by issuer");
    }
    match method {
        "none" => {
            client = client.set_auth_type(AuthType::RequestBody);
        }
        "client_secret_basic" | "client_secret_post" => {
            let secret = secret
                .filter(|value| !value.is_empty())
                .ok_or_else(|| anyhow::anyhow!("OAuth client secret missing"))?;
            client = client
                .set_client_secret(ClientSecret::new(secret))
                .set_auth_type(if method == "client_secret_post" {
                    AuthType::RequestBody
                } else {
                    AuthType::BasicAuth
                });
        }
        _ => bail!(
            "OAuth client authentication requires unsupported signing method; use a supported configured client"
        ),
    }
    Ok(client)
}

pub(super) fn stored_tokens(
    tokens: oauth2::basic::BasicTokenResponse,
    issuer: &str,
    previous: Option<&Value>,
) -> Result<Value> {
    let duration = tokens.expires_in().map(|value| value.as_secs());
    let mut value = serde_json::to_value(tokens)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("invalid OAuth token response"))?;
    if let Some(duration) = duration {
        object.insert(
            "expires_at".to_owned(),
            json!(files::now().saturating_add(duration)),
        );
    }
    if !object.contains_key("refresh_token")
        && let Some(refresh) = previous.and_then(|value| value.get("refresh_token"))
    {
        object.insert("refresh_token".to_owned(), refresh.clone());
    }
    object.insert("issuer".to_owned(), json!(issuer));
    object.insert(
        "__mcporter_generation".to_owned(),
        json!(oauth2::CsrfToken::new_random().secret()),
    );
    store::validate_tokens(&value)?;
    Ok(value)
}

pub(super) async fn refresh(authentication: &HttpAuth) -> Result<String> {
    super::ensure_writable(authentication)?;
    if authentication.cached_only {
        bail!(
            "--no-oauth permits only valid cached tokens; explicitly authorize or retry without --no-oauth"
        );
    }
    let transaction = files::AsyncLocks::new(store::transaction_async(authentication).await?);
    let snapshot = read_snapshot(authentication).await?;
    let tokens = snapshot
        .get("tokens")
        .ok_or_else(|| missing(authentication))?;
    store::validate_tokens(tokens)?;
    if !store::expired(tokens) {
        return Ok(string(tokens, "access_token")
            .ok_or_else(|| missing(authentication))?
            .to_owned());
    }
    let refresh = RefreshToken::new(
        string(tokens, "refresh_token")
            .ok_or_else(|| missing(authentication))?
            .to_owned(),
    );
    let info = snapshot
        .get("clientInfo")
        .filter(|value| !value.is_null())
        .ok_or_else(|| {
            anyhow::anyhow!("cached OAuth client registration missing; explicitly authorize")
        })?;
    let discovery = discover(authentication, &snapshot, false).await?;
    let mut bound = snapshot.clone();
    if let Some(object) = bound.as_object_mut() {
        object.insert("discoveryState".to_owned(), discovery.clone());
    }
    store::validate_binding(authentication, &bound)?;
    let client = client(authentication, info, &discovery)?;
    let mut request = client.exchange_refresh_token(&refresh);
    if let Some(resource) = discovery
        .get("resourceMetadata")
        .and_then(|metadata| string(metadata, "resource"))
    {
        request = request.add_extra_param("resource", resource.to_owned());
    }
    let issuer = discovery
        .get("authorizationServerMetadata")
        .and_then(|metadata| string(metadata, "issuer"))
        .ok_or_else(|| anyhow::anyhow!("OAuth issuer missing"))?;
    let token_issuer = issuer.to_owned();
    let token_client = move |request| token_http(request, token_issuer.clone());
    let tokens_response = request.request_async(&token_client).await
        .map_err(|_| anyhow::anyhow!("OAuth refresh failed or grant rejected; credentials preserved; retry or explicitly authorize"))?;
    let refreshed = stored_tokens(tokens_response, issuer, Some(tokens))?;
    let access_token = string(&refreshed, "access_token")
        .ok_or_else(|| missing(authentication))?
        .to_owned();
    store::save_async(
        authentication,
        json!({"tokens":refreshed,"discoveryState":discovery}),
        Some(snapshot),
    )
    .await?;
    transaction.release().await?;
    Ok(access_token)
}

pub(super) async fn token_http(
    request: oauth2::HttpRequest,
    issuer: String,
) -> Result<oauth2::HttpResponse, std::io::Error> {
    let result: Result<oauth2::HttpResponse> = async {
        let (parts, body) = request.into_parts();
        let mut response = http()?
            .request(parts.method, parts.uri.to_string())
            .headers(parts.headers)
            .body(body)
            .send()
            .await
            .map_err(|_| anyhow::anyhow!("OAuth token request failed"))?;
        let status = response.status();
        let headers = response.headers().clone();
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::anyhow!("OAuth token response could not be read"))?
        {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                bail!("OAuth token response exceeds size limit");
            }
            bytes.extend_from_slice(&chunk);
        }
        if status.is_success() {
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid OAuth token JSON"))?;
            for field in ["issuer", "iss"] {
                if string(&value, field).is_some_and(|received| {
                    received.trim_end_matches('/') != issuer.trim_end_matches('/')
                }) {
                    bail!("OAuth token response issuer mismatch");
                }
            }
        }
        let mut response = oauth2::HttpResponse::new(bytes);
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        Ok(response)
    }
    .await;
    result.map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "OAuth token HTTP transaction failed",
        )
    })
}
