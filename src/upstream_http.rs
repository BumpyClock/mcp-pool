use std::io;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderValue};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::upstream::UpstreamHandle;

#[path = "upstream_http_legacy.rs"]
mod legacy;
#[path = "upstream_http_retire.rs"]
mod retire;
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

struct Request {
    line: String,
    identifier: Option<Value>,
    initialize: bool,
    initialization_barrier: bool,
}

impl Request {
    fn parse(line: String) -> Result<Self, String> {
        if line.len() > sse_parser::FRAME_LIMIT {
            return Err("HTTP request exceeds size limit".into());
        }
        let value: Value =
            serde_json::from_str(&line).map_err(|_| "HTTP request is not valid JSON")?;
        if !value.is_object() {
            return Err("HTTP transport requires a JSON-RPC object".into());
        }
        let method = value.get("method").and_then(Value::as_str);
        let identifier = method.and_then(|_| value.get("id").cloned());
        let initialize = method == Some("initialize");
        let initialization_barrier = initialize || method == Some("notifications/initialized");
        Ok(Self {
            line,
            identifier,
            initialize,
            initialization_barrier,
        })
    }
}

pub async fn spawn(
    url: String,
    sse: bool,
    response_tx: mpsc::Sender<String>,
) -> io::Result<UpstreamHandle> {
    let url = reqwest::Url::parse(&url)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid HTTP upstream URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "HTTP upstream requires an HTTP or HTTPS URL",
        ));
    }
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| io::Error::other("Could not build HTTP upstream client"))?;
    if sse {
        return legacy::spawn(client, url, response_tx).await;
    }
    let (request_tx, request_rx) = mpsc::channel(1024);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (completion_tx, completion) = watch::channel(None);
    tokio::spawn(async move {
        let mut workers = JoinSet::new();
        let session = Arc::new(Mutex::new(Session::Fresh));
        let (result, deliberate) = {
            let work = run(
                client.clone(),
                url.clone(),
                request_rx,
                response_tx,
                session.clone(),
                &mut workers,
            );
            tokio::pin!(work);
            tokio::select! {
                _ = shutdown_rx => (Ok(()), true),
                result = &mut work => (result, false),
            }
        };
        workers.abort_all();
        while workers.join_next().await.is_some() {}
        if result.is_err() {
            crate::diagnostics::log("upstream_http_transport_stopped");
        }
        if deliberate {
            retire::terminate_session(&client, &url, &session).await;
        }
        drop(client);
        completion_tx.send_replace(Some(Ok(())));
    });
    Ok(UpstreamHandle::new(request_tx, shutdown_tx, completion))
}

async fn run(
    client: reqwest::Client,
    url: reqwest::Url,
    mut request_rx: mpsc::Receiver<String>,
    response_tx: mpsc::Sender<String>,
    session: Arc<Mutex<Session>>,
    workers: &mut JoinSet<()>,
) -> Result<(), String> {
    loop {
        tokio::select! {
            biased;
            _ = response_tx.closed() => return Ok(()),
            result = workers.join_next(), if !workers.is_empty() => {
                if result.is_some_and(|result| result.is_err()) {
                    return Err("HTTP request worker failed".into());
                }
            }
            line = request_rx.recv(), if workers.len() < MAX_CONCURRENT_REQUESTS => {
                let Some(line) = line else {
                    while workers.join_next().await.is_some() {}
                    return Ok(());
                };
                let request = match Request::parse(line.clone()) {
                    Ok(request) => request,
                    Err(error) => {
                        let value = serde_json::from_str::<Value>(&line).ok();
                        let identifier = value.as_ref().and_then(|value| value.get("id"));
                        send_error(&response_tx, identifier, &error).await;
                        continue;
                    }
                };
                if request.initialization_barrier {
                    // Initialization establishes the headers every later request must use.
                    if request.initialize {
                        let client = client.clone();
                        let url = url.clone();
                        let response_tx = response_tx.clone();
                        let session = session.clone();
                        let (established, establishment) = oneshot::channel();
                        workers.spawn(async move {
                            execute(
                                &client, &url, request, &response_tx, &session, Some(established),
                            ).await;
                        });
                        establishment.await.map_err(|_| "HTTP initialization worker stopped")?;
                    } else {
                        execute(&client, &url, request, &response_tx, &session, None).await;
                    }
                } else {
                    let client = client.clone();
                    let url = url.clone();
                    let response_tx = response_tx.clone();
                    let session = session.clone();
                    workers.spawn(async move {
                        execute(&client, &url, request, &response_tx, &session, None).await;
                    });
                }
            }
        }
    }
}

async fn execute(
    client: &reqwest::Client,
    url: &reqwest::Url,
    request: Request,
    response_tx: &mpsc::Sender<String>,
    session: &Arc<Mutex<Session>>,
    mut established: Option<oneshot::Sender<()>>,
) {
    let mut answered = false;
    let result = timeout(
        REQUEST_TIMEOUT,
        post(
            client,
            url,
            &request,
            response_tx,
            session,
            &mut answered,
            &mut established,
        ),
    )
    .await
    .unwrap_or_else(|_| Err("HTTP request deadline exceeded; request was not replayed".into()));
    if let Err(error) = result
        && !answered
    {
        send_error(response_tx, request.identifier.as_ref(), &error).await;
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
    established: &mut Option<oneshot::Sender<()>>,
) -> Result<(), String> {
    let state = session.lock().await.clone();
    let mut builder = client
        .post(url.clone())
        .header(CONTENT_TYPE, "application/json")
        .header(ACCEPT, "application/json, text/event-stream");
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
            Ok(())
        } else {
            Err("HTTP upstream returned no JSON-RPC response".into())
        };
    }
    let session_identifier = response.headers().get("Mcp-Session-Id").cloned();
    match content_type(&response).as_str() {
        "text/event-stream" => {
            let mut decoder = sse_parser::Decoder::new();
            while let Some(chunk) = read_chunk(&mut response).await? {
                for event in decoder.feed(&chunk)? {
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
            }
            if request.identifier.is_some() && !*answered {
                Err("HTTP SSE ended before the JSON-RPC response".into())
            } else {
                Ok(())
            }
        }
        "application/json" => {
            let mut body = Vec::new();
            while let Some(chunk) = read_chunk(&mut response).await? {
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
            Ok(())
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
    established: &mut Option<oneshot::Sender<()>>,
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

fn release_initialization(established: &mut Option<oneshot::Sender<()>>) {
    if let Some(established) = established.take()
        && established.send(()).is_err()
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

async fn read_chunk(response: &mut reqwest::Response) -> Result<Option<Vec<u8>>, String> {
    timeout(READ_TIMEOUT, response.chunk())
        .await
        .map_err(|_| "HTTP response read deadline exceeded")?
        .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
        .map_err(|_| "HTTP response read failed; request was not replayed".into())
}
