use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use anyhow::{Result, bail};
use oauth2::{AuthorizationCode, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope};
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::{
    AuthOptions, HttpAuth, checked_url, context, ensure_writable, files, protocol, read_snapshot,
    setting, store, string,
};
use crate::server_config::ConfiguredServer;

pub(super) async fn authorize(
    server: &ConfiguredServer,
    options: AuthOptions,
    timeout_ms: Option<u64>,
) -> Result<Value> {
    let no_browser = options.no_browser
        || std::env::var("MCPORTER_OAUTH_NO_BROWSER").is_ok_and(|value| value == "1");
    let json_output = options.json;
    authorize_with_observer(server, options, timeout_ms, move |url, redirect| async move {
        if no_browser {
            if json_output {
                println!("{}", json!({"status":"authorization_required","authorizationUrl":url.as_str(),"redirectUrl":redirect.as_str()}));
            } else {
                eprintln!("Open this authorization URL, then complete the loopback callback:\n{url}");
            }
            Ok(())
        } else {
            super::browser::open(&url).await
        }
    }).await
}

pub(super) async fn authorize_with_observer<F, Fut>(
    server: &ConfiguredServer,
    options: AuthOptions,
    timeout_ms: Option<u64>,
    observer: F,
) -> Result<Value>
where
    F: FnOnce(Url, Url) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let authentication = context(server)?;
    ensure_writable(&authentication)?;
    let transaction = files::AsyncLocks::new(store::transaction_async(&authentication).await?);
    let snapshot = if options.reset {
        let clone = authentication.clone();
        tokio::task::spawn_blocking(move || store::clear(&clone)).await??;
        json!({})
    } else {
        let snapshot = read_snapshot(&authentication).await?;
        store::validate_binding(&authentication, &snapshot)?;
        if let Some(tokens) = snapshot.get("tokens")
            && store::validate_tokens(tokens).is_ok()
            && !store::expired(tokens)
        {
            return Ok(json!({"server":server.name,"status":"authenticated","reused":true}));
        }
        snapshot
    };
    let duration = Duration::from_millis(timeout_ms.unwrap_or(authentication.timeout_ms));
    let result = tokio::select! {
        result = tokio::time::timeout(duration, exchange(server, &authentication, snapshot, observer)) => {
            result.map_err(|_| anyhow::anyhow!("OAuth authorization timed out; rerun explicit auth"))?
        }
        result = tokio::signal::ctrl_c() => {
            result.map_err(|_| anyhow::anyhow!("could not install OAuth cancellation handler"))?;
            Err(anyhow::anyhow!("OAuth authorization cancelled; existing credentials preserved"))
        }
    };
    transaction.release().await?;
    result
}

async fn exchange<F, Fut>(
    server: &ConfiguredServer,
    authentication: &HttpAuth,
    snapshot: Value,
    observer: F,
) -> Result<Value>
where
    F: FnOnce(Url, Url) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let discovery = protocol::discover(authentication, &snapshot, true).await?;
    let metadata = discovery
        .get("authorizationServerMetadata")
        .ok_or_else(|| anyhow::anyhow!("OAuth metadata missing"))?;
    if !metadata
        .get("code_challenge_methods_supported")
        .and_then(Value::as_array)
        .is_some_and(|methods| methods.iter().any(|value| value.as_str() == Some("S256")))
    {
        bail!("OAuth issuer does not advertise PKCE S256; insecure authorization is refused");
    }
    let issuer =
        string(metadata, "issuer").ok_or_else(|| anyhow::anyhow!("OAuth issuer missing"))?;
    let (listener, redirect) = listener(authentication, &snapshot).await?;
    let client_info =
        registration(server, authentication, &snapshot, &discovery, &redirect).await?;
    let client = protocol::client(authentication, &client_info, &discovery)?.set_redirect_uri(
        RedirectUrl::new(redirect.to_string())
            .map_err(|_| anyhow::anyhow!("invalid OAuth redirect"))?,
    );
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut request = client
        .authorize_url(CsrfToken::new_random)
        .set_pkce_challenge(challenge);
    let scopes = authentication
        .scope
        .as_deref()
        .map(str::to_owned)
        .or_else(|| {
            discovery
                .get("resourceMetadata")
                .and_then(|metadata| metadata.get("scopes_supported"))
                .or_else(|| metadata.get("scopes_supported"))
                .and_then(Value::as_array)
                .map(|scopes| {
                    scopes
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
        });
    if let Some(scopes) = scopes {
        for scope in scopes.split_whitespace() {
            request = request.add_scope(Scope::new(scope.to_owned()));
        }
    }
    let resource = discovery
        .get("resourceMetadata")
        .and_then(|metadata| string(metadata, "resource"));
    if let Some(resource) = resource {
        request = request.add_extra_param("resource", resource.to_owned());
    }
    let (url, state) = request.url();
    let pending_client = if snapshot.get("tokens").is_some() {
        snapshot.get("clientInfo").cloned().unwrap_or(Value::Null)
    } else {
        client_info.clone()
    };
    let pending = json!({
        "discoveryState":discovery,"authorizationServerUrl":issuer,
        "resourceUrl":resource,
        "clientInfo":pending_client,"state":state.secret(),"codeVerifier":verifier.secret(),
    });
    store::save_async(authentication, pending, Some(snapshot)).await?;
    let pending_snapshot = read_snapshot(authentication).await?;
    observer(url, redirect.clone()).await?;
    let require_issuer = metadata
        .get("authorization_response_iss_parameter_supported")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let code = callback(&listener, &redirect, state.secret(), issuer, require_issuer).await?;
    let mut request = client
        .exchange_code(AuthorizationCode::new(code))
        .set_pkce_verifier(verifier);
    if let Some(resource) = resource {
        request = request.add_extra_param("resource", resource.to_owned());
    }
    let token_issuer = issuer.to_owned();
    let token_client = move |request| protocol::token_http(request, token_issuer.clone());
    let response = request.request_async(&token_client).await.map_err(|_| {
        anyhow::anyhow!("OAuth authorization-code exchange failed; credentials preserved")
    })?;
    let tokens = protocol::stored_tokens(response, issuer, None)?;
    let patch = json!({"tokens":tokens,"clientInfo":client_info,"state":null,"codeVerifier":null});
    store::save_async(authentication, patch, Some(pending_snapshot)).await?;
    Ok(json!({"server":server.name,"status":"authenticated","reused":false}))
}

async fn listener(authentication: &HttpAuth, snapshot: &Value) -> Result<(TcpListener, Url)> {
    let saved = snapshot
        .get("clientInfo")
        .and_then(|info| info.get("redirect_uris"))
        .and_then(Value::as_array)
        .and_then(|redirects| redirects.first())
        .and_then(Value::as_str);
    let candidate = authentication
        .redirect_url
        .as_deref()
        .or(saved)
        .unwrap_or("http://127.0.0.1:0/callback");
    let mut redirect = checked_url(candidate)?;
    if redirect.scheme() != "http" || redirect.query().is_some() || redirect.fragment().is_some() {
        bail!("OAuth callback must be an HTTP loopback URL without query or fragment");
    }
    let host = match redirect.host_str() {
        Some("127.0.0.1" | "localhost") => IpAddr::V4(Ipv4Addr::LOCALHOST),
        Some("[::1]" | "::1") => IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        _ => bail!("OAuth callback must bind only to loopback"),
    };
    let port = redirect.port().unwrap_or(80);
    let listener = TcpListener::bind(SocketAddr::new(host, port)).await
        .map_err(|_| anyhow::anyhow!("registered OAuth loopback callback port unavailable; stop its owner or explicitly reset registration"))?;
    redirect
        .set_port(Some(listener.local_addr()?.port()))
        .map_err(|_| anyhow::anyhow!("invalid loopback redirect port"))?;
    Ok((listener, redirect))
}

async fn registration(
    server: &ConfiguredServer,
    authentication: &HttpAuth,
    snapshot: &Value,
    discovery: &Value,
    redirect: &Url,
) -> Result<Value> {
    let issuer = discovery
        .get("authorizationServerMetadata")
        .and_then(|metadata| string(metadata, "issuer"))
        .ok_or_else(|| anyhow::anyhow!("OAuth issuer missing"))?;
    if let Some(info) = snapshot.get("clientInfo")
        && string(info, "client_id").is_some()
    {
        if string(info, "issuer")
            .is_some_and(|stored| stored.trim_end_matches('/') != issuer.trim_end_matches('/'))
        {
            bail!("cached OAuth client issuer mismatch; explicitly reset registration");
        }
        if let Some(redirects) = info.get("redirect_uris").and_then(Value::as_array)
            && !redirects
                .iter()
                .any(|value| value.as_str() == Some(redirect.as_str()))
        {
            bail!("OAuth registered redirect differs; explicitly reset registration");
        }
        let mut info = info.clone();
        configured_secret(server, authentication, &mut info)?;
        return Ok(info);
    }
    if let Some(identifier) = authentication
        .client_id
        .as_ref()
        .or(authentication.client_metadata_url.as_ref())
    {
        if authentication.client_metadata_url.is_some() && authentication.client_id.is_none() {
            let url = checked_url(identifier)?;
            if url.scheme() != "https" || url.query().is_some() {
                bail!("OAuth client metadata document requires a public HTTPS URL");
            }
            if !discovery
                .get("authorizationServerMetadata")
                .and_then(|metadata| metadata.get("client_id_metadata_document_supported"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                bail!("issuer does not support OAuth client metadata documents");
            }
        }
        let mut info = json!({"client_id":identifier,"issuer":issuer,"redirect_uris":[redirect.as_str()],
            "token_endpoint_auth_method":authentication.token_auth_method.as_deref().unwrap_or(
                if authentication.client_secret_env.is_some() || setting(&server.raw, "oauthClientSecret", "oauth_client_secret").is_some() {
                    "client_secret_basic"
                } else { "none" })});
        configured_secret(server, authentication, &mut info)?;
        return Ok(info);
    }
    let metadata = discovery
        .get("authorizationServerMetadata")
        .ok_or_else(|| anyhow::anyhow!("OAuth metadata missing"))?;
    let endpoint = checked_url(string(metadata, "registration_endpoint")
        .ok_or_else(|| anyhow::anyhow!("issuer has no dynamic registration endpoint; configure a legitimate oauthClientId or supported oauthClientMetadataUrl"))?)?;
    let payload = json!({
        "client_name":authentication.client_name,"redirect_uris":[redirect.as_str()],
        "grant_types":["authorization_code","refresh_token"],"response_types":["code"],
        "token_endpoint_auth_method":"none",
    });
    let response = protocol::http()?
        .post(endpoint)
        .json(&payload)
        .send()
        .await
        .map_err(|_| anyhow::anyhow!("OAuth client registration request failed"))?;
    let mut info = protocol::json_response(response).await?;
    if string(&info, "client_id").is_none() {
        bail!("OAuth registration did not return client_id");
    }
    if let Some(object) = info.as_object_mut() {
        object.insert("issuer".to_owned(), json!(issuer));
        object.insert(
            "__mcporter_client_generation".to_owned(),
            json!(CsrfToken::new_random().secret()),
        );
    }
    if !info
        .get("redirect_uris")
        .and_then(Value::as_array)
        .is_some_and(|values| {
            values
                .iter()
                .any(|value| value.as_str() == Some(redirect.as_str()))
        })
    {
        bail!("OAuth registration response did not preserve requested redirect URI");
    }
    Ok(info)
}

fn configured_secret(
    server: &ConfiguredServer,
    authentication: &HttpAuth,
    info: &mut Value,
) -> Result<()> {
    let secret = if let Some(variable) = &authentication.client_secret_env {
        Some(
            server
                .definition
                .env
                .get(variable)
                .cloned()
                .or_else(|| std::env::var(variable).ok())
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    anyhow::anyhow!("configured OAuth client-secret environment variable missing")
                })?,
        )
    } else {
        setting(&server.raw, "oauthClientSecret", "oauth_client_secret").map(str::to_owned)
    };
    if let Some(secret) = secret
        && let Some(object) = info.as_object_mut()
    {
        object.insert("client_secret".to_owned(), json!(secret));
    }
    Ok(())
}

pub(super) async fn callback(
    listener: &TcpListener,
    redirect: &Url,
    expected_state: &str,
    issuer: &str,
    require_issuer: bool,
) -> Result<String> {
    loop {
        let (mut stream, peer) = listener.accept().await?;
        if !peer.ip().is_loopback() {
            continue;
        }
        let mut bytes = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), async {
            let mut buffer = [0_u8; 1024];
            while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).await?;
                if count == 0 || bytes.len() + count > 8192 {
                    return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
                }
                bytes.extend_from_slice(
                    buffer
                        .get(..count)
                        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidData))?,
                );
            }
            Ok::<(), std::io::Error>(())
        })
        .await;
        if !matches!(read, Ok(Ok(()))) {
            continue;
        }
        let request = String::from_utf8_lossy(&bytes);
        let mut parts = request
            .lines()
            .next()
            .unwrap_or_default()
            .split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();
        let host = request.lines().find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("host"))
                .map(|(_, value)| value.trim())
        });
        let expected_host = match redirect.port() {
            Some(port) => format!("{}:{port}", redirect.host_str().unwrap_or_default()),
            None => redirect.host_str().unwrap_or_default().to_owned(),
        };
        let parsed = redirect.join(target);
        let result = if method != "GET"
            || !target.starts_with('/')
            || target.starts_with("//")
            || host != Some(expected_host.as_str())
        {
            Err(anyhow::anyhow!("invalid OAuth callback origin"))
        } else if let Ok(url) = parsed {
            if url.path() != redirect.path() {
                Err(anyhow::anyhow!("invalid OAuth callback path"))
            } else {
                callback_query(&url, expected_state, issuer, require_issuer)
            }
        } else {
            Err(anyhow::anyhow!("invalid OAuth callback URL"))
        };
        let valid = result.is_ok();
        let response = if valid {
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nAuthorization received. You may close this."
        } else {
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nConnection: close\r\n\r\nInvalid OAuth callback URL."
        };
        stream.write_all(response.as_bytes()).await?;
        stream.shutdown().await?;
        // A denied consent must terminate; unsolicited invalid callbacks do not.
        if valid {
            return result;
        }
        if parsed_query_error(target, expected_state) {
            return Err(anyhow::anyhow!(
                "OAuth consent denied by authorization server"
            ));
        }
    }
}

fn parsed_query_error(target: &str, expected_state: &str) -> bool {
    Url::parse(&format!("http://127.0.0.1{target}"))
        .ok()
        .is_some_and(|url| {
            url.query_pairs()
                .any(|(key, value)| key == "state" && value == expected_state)
                && url.query_pairs().any(|(key, _)| key == "error")
        })
}

pub(super) fn callback_query(
    url: &Url,
    expected_state: &str,
    issuer: &str,
    require_issuer: bool,
) -> Result<String> {
    let pairs: Vec<_> = url.query_pairs().collect();
    let single = |name: &str| -> Result<Option<String>> {
        let mut values = pairs
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.to_string());
        let first = values.next();
        if values.next().is_some() {
            bail!("duplicate OAuth callback parameter");
        }
        Ok(first)
    };
    let state = single("state")?.ok_or_else(|| anyhow::anyhow!("OAuth callback missing state"))?;
    if Sha256::digest(state.as_bytes()) != Sha256::digest(expected_state.as_bytes()) {
        bail!("OAuth callback state mismatch");
    }
    if let Some(received) = single("iss")? {
        if received.trim_end_matches('/') != issuer.trim_end_matches('/') {
            bail!("OAuth callback issuer mismatch");
        }
    } else if require_issuer {
        bail!("OAuth callback missing required issuer");
    }
    if single("error")?.is_some() {
        bail!("OAuth authorization denied");
    }
    single("code")?
        .filter(|code| !code.is_empty())
        .ok_or_else(|| anyhow::anyhow!("OAuth callback missing authorization code"))
}
