use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn execute(
    client: &reqwest::Client,
    url: &reqwest::Url,
    mut request: Request,
    response_tx: &mpsc::Sender<String>,
    session: &Arc<Mutex<Session>>,
    mut established: Option<oneshot::Sender<Option<Request>>>,
    options: &Options,
    mode: ExchangeMode,
) {
    let mut answered = false;
    let budget = request.budget.clone();
    let result = budget
        .wait(post(
            client,
            url,
            &mut request,
            response_tx,
            session,
            &mut answered,
            &mut established,
            options,
            mode,
        ))
        .await
        .unwrap_or_else(|_| {
            Err(HttpFailure::new(
                FailureKind::Timeout,
                "HTTP request deadline exceeded; request was not replayed",
            ))
        });
    if let Ok(PostOutcome::DiscoveryUnsupported(status)) = result {
        if let Some(identifier) = request.identifier {
            let payload = serde_json::json!({
                "jsonrpc":"2.0",
                "id":identifier,
                "error":{
                    "code":crate::mcp_protocol::DISCOVERY_UNSUPPORTED,
                    "message":"HTTP endpoint does not support modern MCP discovery",
                    "data":crate::mcp_protocol::discovery_rejection_data(status)
                }
            });
            if response_tx.send(payload.to_string()).await.is_err() {
                options.log.log("upstream_http_response_receiver_closed");
            }
        }
    } else if matches!(result, Ok(PostOutcome::LegacyMismatch)) {
        if let Some(established) = established.take()
            && established.send(Some(request)).is_err()
        {
            options.log.log("upstream_http_fallback_waiter_closed");
        }
    } else if let Err(error) = result {
        options
            .log
            .log(format!("upstream_http_request_failed: {error}"));
        if !answered {
            send_failure(
                response_tx,
                request.identifier.as_ref(),
                &error,
                &options.log,
            )
            .await;
        }
    }
    release_initialization(&mut established, &options.log);
}

#[allow(clippy::too_many_arguments)]
async fn post(
    client: &reqwest::Client,
    url: &reqwest::Url,
    request: &mut Request,
    response_tx: &mpsc::Sender<String>,
    session: &Arc<Mutex<Session>>,
    answered: &mut bool,
    established: &mut Option<oneshot::Sender<Option<Request>>>,
    options: &Options,
    mode: ExchangeMode,
) -> Result<PostOutcome, HttpFailure> {
    let state = session.lock().await.clone();
    let fresh = matches!(state, Session::Fresh);
    let mut builder =
        options
            .authorize(client.post(url.clone()))
            .await?
            .headers(HeaderMap::from_iter([
                (CONTENT_TYPE, HeaderValue::from_static("application/json")),
                (
                    ACCEPT,
                    HeaderValue::from_static("application/json, text/event-stream"),
                ),
            ]));
    match state {
        Session::Expired => {
            return Err(
                "MCP HTTP session expired; restart the upstream before sending requests".into(),
            );
        }
        Session::Ready {
            identifier,
            protocol,
        } => {
            if request.kind.is_initialize() {
                return Err("MCP HTTP upstream is already initialized".into());
            }
            if (protocol == crate::mcp_protocol::MODERN_VERSION) != request.kind.is_modern() {
                return Err("MCP request protocol does not match the shared upstream".into());
            }
            builder = builder.header(crate::mcp_protocol::PROTOCOL_VERSION_HEADER, protocol);
            if let Some(identifier) = identifier {
                builder = builder.header(crate::mcp_protocol::SESSION_ID_HEADER, identifier);
            }
        }
        Session::Fresh => {}
    }
    if request.kind.is_modern() {
        builder = builder.header(
            crate::mcp_protocol::PROTOCOL_VERSION_HEADER,
            crate::mcp_protocol::MODERN_VERSION,
        );
        if let Some(method) = &request.method {
            builder = builder.header(crate::mcp_protocol::METHOD_HEADER, method);
        }
        if let Some(name) = &request.name {
            builder = builder.header(crate::mcp_protocol::NAME_HEADER, name);
        }
    }
    let body = if mode == ExchangeMode::InitialLegacy && fresh {
        request.line.clone()
    } else {
        std::mem::take(&mut request.line)
    };
    let mut response = builder.body(body).send().await.map_err(|_| {
        HttpFailure::new(
            FailureKind::Offline,
            "HTTP request failed; request was not replayed",
        )
    })?;
    let status = response.status();
    if request.kind.is_discovery()
        && fresh
        && crate::mcp_protocol::discovery_http_rejection(u64::from(status.as_u16()))
    {
        return Ok(PostOutcome::DiscoveryUnsupported(status.as_u16()));
    }
    if mode == ExchangeMode::InitialLegacy
        && fresh
        && crate::mcp_protocol::legacy_http_rejection(status.as_u16())
    {
        return Ok(PostOutcome::LegacyMismatch);
    }
    if status == reqwest::StatusCode::NOT_FOUND
        && matches!(
            &*session.lock().await,
            Session::Ready {
                identifier: Some(_),
                ..
            }
        )
    {
        *session.lock().await = Session::Expired;
        return Err(HttpFailure::status(
            404,
            "MCP HTTP session expired (HTTP 404); request was not replayed",
        ));
    }
    if !status.is_success() {
        return Err(HttpFailure::status(
            status.as_u16(),
            format!(
                "HTTP upstream returned status {}; request was not replayed",
                status.as_u16()
            ),
        ));
    }
    if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::NO_CONTENT {
        return if request.identifier.is_none() {
            Ok(PostOutcome::Complete)
        } else {
            Err("HTTP upstream returned no JSON-RPC response".into())
        };
    }
    let session_identifier = response
        .headers()
        .get(crate::mcp_protocol::SESSION_ID_HEADER)
        .cloned();
    match content_type(&response).as_str() {
        "text/event-stream" => {
            let mut decoder = sse_parser::Decoder::new();
            while let Some(chunk) = request.read_chunk(&mut response, options).await? {
                let feed = decoder.feed(&chunk);
                for event in feed.events {
                    if event.name.is_empty() || event.name == "message" {
                        deliver(
                            &event.data,
                            request,
                            session_identifier.clone(),
                            session,
                            response_tx,
                            answered,
                            established,
                            &options.log,
                        )
                        .await?;
                    }
                }
                if let Some(error) = feed.error {
                    return Err(error.into());
                }
            }
            if request.identifier.is_some() && !*answered {
                Err("HTTP SSE ended before the JSON-RPC response".into())
            } else {
                Ok(PostOutcome::Complete)
            }
        }
        "application/json" => {
            let mut body = Vec::new();
            while let Some(chunk) = request.read_chunk(&mut response, options).await? {
                if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
                    return Err(format!(
                        "HTTP JSON response exceeds size limit ({MAX_RESPONSE_BYTES} bytes)"
                    )
                    .into());
                }
                body.extend_from_slice(&chunk);
            }
            let body =
                std::str::from_utf8(&body).map_err(|_| "HTTP response contains invalid UTF-8")?;
            deliver(
                body,
                request,
                session_identifier,
                session,
                response_tx,
                answered,
                established,
                &options.log,
            )
            .await?;
            if request.identifier.is_some() && !*answered {
                return Err("HTTP response did not match the JSON-RPC request ID".into());
            }
            Ok(PostOutcome::Complete)
        }
        _ => Err("HTTP upstream returned an unsupported content type".into()),
    }
}
