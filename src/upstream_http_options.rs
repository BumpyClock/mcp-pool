use super::*;

#[derive(Clone)]
pub(super) struct Options {
    pub(super) log: ServerLog,
    headers: HeaderMap,
    auth: Option<crate::oauth::HttpAuth>,
    pub(super) request_timeout: Duration,
    pub(super) read_timeout: Duration,
}

impl Options {
    pub(super) fn new(
        headers: BTreeMap<String, String>,
        timeout_ms: Option<u64>,
        auth: Option<crate::oauth::HttpAuth>,
        log: ServerLog,
    ) -> io::Result<Self> {
        let log = log.with_redactions(
            headers.values().cloned().chain(
                auth.iter()
                    .flat_map(|auth| [auth.server_url.clone(), auth.identity_key.clone()]),
            ),
        );
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid configured HTTP headers",
            )
        };
        let mut configured = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| invalid())?;
            if auth.is_some() && name == AUTHORIZATION {
                continue;
            }
            if matches!(
                name.as_str(),
                "mcp-session-id" | "mcp-protocol-version" | "mcp-method" | "mcp-name"
            ) {
                continue;
            }
            let mut value = HeaderValue::from_str(&value).map_err(|_| invalid())?;
            value.set_sensitive(true);
            configured.insert(name, value);
        }
        if timeout_ms == Some(0) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "HTTP deadline must be positive",
            ));
        }
        Ok(Self {
            log,
            headers: configured,
            auth,
            request_timeout: configured_request_timeout(timeout_ms),
            read_timeout: timeout_ms
                .map(Duration::from_millis)
                .unwrap_or(READ_TIMEOUT),
        })
    }

    pub(super) fn validate_origin(&self, url: &reqwest::Url) -> io::Result<()> {
        if let Some(auth) = &self.auth {
            let matches = reqwest::Url::parse(&auth.server_url)
                .ok()
                .is_some_and(|bound| bound.origin() == url.origin());
            if !matches {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "HTTP authorization origin does not match upstream",
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn authorize(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, HttpFailure> {
        let mut builder = builder.headers(self.headers.clone());
        if let Some(auth) = &self.auth {
            let value = crate::oauth::authorization_header(auth)
                .await
                .map_err(|_| {
                    let message = if auth.read_only {
                        "HTTP authorization unavailable: read-only policy blocks refresh; remove MCP_CLI_CREDENTIALS_READ_ONLY and MCP_POOL_CREDENTIALS_READ_ONLY only for explicitly authorized `mcp-cli auth SERVER`"
                    } else if auth.cached_only {
                        "HTTP authorization unavailable: --no-oauth permits valid cached credentials only; run `mcp-cli auth SERVER` explicitly before retrying"
                    } else {
                        "HTTP authorization could not be refreshed"
                    };
                    HttpFailure::new(FailureKind::Auth, message)
                })?;
            if let Some(value) = value {
                let mut value = HeaderValue::from_str(&value).map_err(|_| {
                    HttpFailure::new(FailureKind::Auth, "HTTP authorization header is invalid")
                })?;
                value.set_sensitive(true);
                builder = builder.header(AUTHORIZATION, value);
            }
        }
        Ok(builder)
    }
}
