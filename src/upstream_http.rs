use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::upstream::UpstreamHandle;

#[path = "upstream_http_legacy.rs"]
mod legacy;
#[path = "upstream_http_options.rs"]
mod options;
use options::Options;
#[path = "upstream_http_request.rs"]
mod request;
use request::Request;
#[cfg(test)]
#[path = "upstream_http_auth_tests.rs"]
mod auth_tests;
#[cfg(test)]
#[path = "upstream_http_deadline_tests.rs"]
mod deadline_tests;
#[cfg(test)]
#[path = "upstream_http_options_tests.rs"]
mod options_tests;
#[path = "upstream_http_retire.rs"]
mod retire;
#[path = "upstream_http_runtime.rs"]
mod runtime;
#[path = "upstream_http_sse.rs"]
mod sse_parser;
#[cfg(test)]
#[path = "upstream_http_tests.rs"]
mod tests;
#[cfg(test)]
#[path = "upstream_http_tests_more.rs"]
mod tests_more;
#[cfg(test)]
#[path = "upstream_http_tests_retirement.rs"]
mod tests_retirement;

#[cfg(not(test))]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
#[cfg(test)]
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(not(test))]
const READ_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(test)]
const READ_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CONCURRENT_REQUESTS: usize = 32;

pub(crate) fn configured_request_timeout(timeout_ms: Option<u64>) -> Duration {
    timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(REQUEST_TIMEOUT)
}

#[derive(Clone, Default)]
enum Session {
    #[default]
    Fresh,
    Ready {
        identifier: Option<HeaderValue>,
        protocol: HeaderValue,
    },
    Expired,
}

enum PostOutcome {
    Complete,
    LegacyMismatch,
}

#[cfg(test)]
pub async fn spawn(
    url: String,
    sse: bool,
    response_tx: mpsc::Sender<String>,
) -> io::Result<UpstreamHandle> {
    spawn_configured(url, sse, BTreeMap::new(), None, None, response_tx).await
}

/// Private caller deadlines include authorization refresh; shared requests retain the configured floor.
/// MCP session/version and content headers remain transport-owned.
pub async fn spawn_configured(
    url: String,
    sse: bool,
    headers: BTreeMap<String, String>,
    timeout_ms: Option<u64>,
    auth: Option<crate::oauth::HttpAuth>,
    response_tx: mpsc::Sender<String>,
) -> io::Result<UpstreamHandle> {
    let options = Options::new(headers, timeout_ms, auth)?;
    let url = reqwest::Url::parse(&url)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid HTTP upstream URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HTTP upstream requires an HTTP or HTTPS URL",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HTTP upstream URL must not contain credentials",
        ));
    }
    options.validate_origin(&url)?;
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .map_err(|_| io::Error::other("Could not build HTTP upstream client"))?;
    if sse {
        return legacy::spawn(client, url, response_tx, options, None).await;
    }
    let (request_tx, request_rx) = mpsc::channel(1024);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (completion_tx, completion) = watch::channel(None);
    tokio::spawn(async move {
        let mut workers = JoinSet::new();
        let mut fallback = None;
        let session = Arc::new(Mutex::new(Session::Fresh));
        let (result, deliberate) = {
            let work = runtime::run(
                client.clone(),
                url.clone(),
                request_rx,
                response_tx,
                session.clone(),
                &mut workers,
                &options,
                &mut fallback,
            );
            tokio::pin!(work);
            tokio::select! {
                _ = shutdown_rx => (Ok(()), true),
                result = &mut work => (result, false),
            }
        };
        workers.abort_all();
        while workers.join_next().await.is_some() {}
        let retirement = if let Some(mut handle) = fallback {
            handle.shutdown().await.map_err(|error| error.to_string())
        } else {
            Ok(())
        };
        if result.is_err() {
            crate::diagnostics::log("upstream_http_transport_stopped");
        }
        if deliberate {
            retire::terminate_session(&client, &url, &session, &options).await;
        }
        drop(client);
        completion_tx.send_replace(Some(retirement));
    });
    Ok(UpstreamHandle::new(request_tx, shutdown_tx, completion))
}

#[allow(clippy::too_many_arguments)]
async fn execute(
    client: &reqwest::Client,
    url: &reqwest::Url,
    request: Request,
    response_tx: &mpsc::Sender<String>,
    session: &Arc<Mutex<Session>>,
    mut established: Option<oneshot::Sender<Option<Request>>>,
    options: &Options,
    allow_fallback: bool,
) {
    let mut answered = false;
    let Some(budget) = &request.budget else {
        send_error(
            response_tx,
            request.identifier.as_ref(),
            "HTTP request budget missing",
        )
        .await;
        release_initialization(&mut established);
        return;
    };
    let result = budget
        .wait(post(
            client,
            url,
            &request,
            response_tx,
            session,
            &mut answered,
            &mut established,
            options,
            allow_fallback,
        ))
        .await
        .unwrap_or_else(|_| Err("HTTP request deadline exceeded; request was not replayed".into()));
    if matches!(result, Ok(PostOutcome::LegacyMismatch)) {
        if let Some(established) = established.take()
            && established.send(Some(request)).is_err()
        {
            crate::diagnostics::log("upstream_http_fallback_waiter_closed");
        }
    } else if let Err(error) = result {
        crate::diagnostics::log(format!("upstream_http_request_failed: {error}"));
        if !answered {
            send_error(response_tx, request.identifier.as_ref(), &error).await;
        }
    }
    release_initialization(&mut established);
}

#[allow(clippy::too_many_arguments)]
async fn post(
    client: &reqwest::Client,
    url: &reqwest::Url,
    request: &Request,
    response_tx: &mpsc::Sender<String>,
    session: &Arc<Mutex<Session>>,
    answered: &mut bool,
    established: &mut Option<oneshot::Sender<Option<Request>>>,
    options: &Options,
    allow_fallback: bool,
) -> Result<PostOutcome, String> {
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
            if request.initialize {
                return Err("MCP HTTP upstream is already initialized".into());
            }
            builder = builder.header("MCP-Protocol-Version", protocol);
            if let Some(identifier) = identifier {
                builder = builder.header("Mcp-Session-Id", identifier);
            }
        }
        Session::Fresh => {}
    }
    let mut response = builder
        .body(request.line.clone())
        .send()
        .await
        .map_err(|_| "HTTP request failed; request was not replayed")?;
    let status = response.status();
    if allow_fallback
        && fresh
        && matches!(
            status,
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
        )
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
        return Err("MCP HTTP session expired (HTTP 404); request was not replayed".into());
    }
    if !status.is_success() {
        return Err(format!(
            "HTTP upstream returned status {}; request was not replayed",
            status.as_u16()
        ));
    }
    if status == reqwest::StatusCode::ACCEPTED || status == reqwest::StatusCode::NO_CONTENT {
        return if request.identifier.is_none() {
            Ok(PostOutcome::Complete)
        } else {
            Err("HTTP upstream returned no JSON-RPC response".into())
        };
    }
    let session_identifier = response.headers().get("Mcp-Session-Id").cloned();
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
                        )
                        .await?;
                    }
                }
                if let Some(error) = feed.error {
                    return Err(error);
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
                if body.len() + chunk.len() > sse_parser::FRAME_LIMIT {
                    return Err("HTTP JSON response exceeds size limit".into());
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

#[allow(clippy::too_many_arguments)]
async fn deliver(
    body: &str,
    request: &Request,
    session_identifier: Option<HeaderValue>,
    session: &Arc<Mutex<Session>>,
    response_tx: &mpsc::Sender<String>,
    answered: &mut bool,
    established: &mut Option<oneshot::Sender<Option<Request>>>,
) -> Result<(), String> {
    let value = parse_message(body)?;
    let matches = request.identifier.as_ref().is_some_and(|identifier| {
        value.get("id") == Some(identifier)
            && (value.get("result").is_some() || value.get("error").is_some())
    });
    if matches && request.initialize && value.get("result").is_some() {
        if session_identifier.as_ref().is_some_and(|identifier| {
            identifier.is_empty()
                || !identifier
                    .as_bytes()
                    .iter()
                    .all(|byte| (0x21..=0x7e).contains(byte))
        }) {
            return Err("MCP initialize response contains an invalid session ID".into());
        }
        let protocol = value
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str)
            .ok_or("MCP initialize response omitted protocolVersion")?;
        if protocol.is_empty() {
            return Err("MCP initialize response contains an invalid protocolVersion".into());
        }
        let protocol = HeaderValue::from_str(protocol)
            .map_err(|_| "MCP initialize response contains an invalid protocolVersion")?;
        *session.lock().await = Session::Ready {
            identifier: session_identifier,
            protocol,
        };
    }
    response_tx
        .send(value.to_string())
        .await
        .map_err(|_| "HTTP response receiver closed")?;
    *answered |= matches;
    if matches {
        release_initialization(established);
    }
    Ok(())
}

fn release_initialization(established: &mut Option<oneshot::Sender<Option<Request>>>) {
    if let Some(established) = established.take()
        && established.send(None).is_err()
    {
        crate::diagnostics::log("upstream_http_initialization_waiter_closed");
    }
}

fn parse_message(body: &str) -> Result<Value, String> {
    let value: Value =
        serde_json::from_str(body).map_err(|_| "HTTP upstream returned invalid JSON")?;
    if !value.is_object()
        || value.get("jsonrpc").and_then(Value::as_str) != Some("2.0")
        || !(value.get("method").and_then(Value::as_str).is_some()
            || (value.get("id").is_some()
                && (value.get("result").is_some() || value.get("error").is_some())))
    {
        return Err("HTTP upstream returned an invalid JSON-RPC message".into());
    }
    Ok(value)
}

async fn send_error(response_tx: &mpsc::Sender<String>, identifier: Option<&Value>, error: &str) {
    if let Some(identifier) = identifier {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": identifier,
            "error": {"code": -32000, "message": error}
        });
        if response_tx.send(payload.to_string()).await.is_err() {
            crate::diagnostics::log("upstream_http_response_receiver_closed");
        }
    } else {
        crate::diagnostics::log("upstream_http_notification_failed");
    }
}

fn content_type(response: &reqwest::Response) -> String {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

async fn read_chunk_with_timeout(
    response: &mut reqwest::Response,
    deadline: Duration,
) -> Result<Option<Vec<u8>>, String> {
    timeout(deadline, response.chunk())
        .await
        .map_err(|_| "HTTP response read deadline exceeded")?
        .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
        .map_err(|_| "HTTP response read failed; request was not replayed".into())
}

async fn read_chunk_with_deadline(
    response: &mut reqwest::Response,
    deadline: &crate::request_deadline::SharedDeadline,
) -> Result<Option<Vec<u8>>, String> {
    deadline
        .wait(response.chunk())
        .await
        .map_err(|_| "HTTP response read deadline exceeded")?
        .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
        .map_err(|_| "HTTP response read failed; request was not replayed".into())
}
