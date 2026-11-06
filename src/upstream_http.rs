use std::collections::BTreeMap;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::diagnostics::ServerLog;
use crate::mcp_protocol::{FailureKind, TransportFailure};
use crate::upstream::UpstreamHandle;

#[path = "upstream_http_exchange.rs"]
mod exchange;
use exchange::execute;
#[path = "upstream_http_legacy.rs"]
mod legacy;
#[path = "upstream_http_options.rs"]
mod options;
use options::Options;
#[path = "upstream_http_request.rs"]
mod request;
use request::{ExchangeMode, Negotiation, Request};
#[path = "upstream_http_response.rs"]
mod response;
use response::deliver;
#[cfg(test)]
#[path = "upstream_http_auth_tests.rs"]
mod auth_tests;
#[cfg(test)]
#[path = "upstream_http_deadline_tests.rs"]
mod deadline_tests;
#[cfg(test)]
#[path = "upstream_http_options_tests.rs"]
mod options_tests;
#[cfg(test)]
#[path = "upstream_http_ownership_tests.rs"]
mod ownership_tests;
#[cfg(test)]
#[path = "upstream_http_protocol_tests.rs"]
mod protocol_tests;
#[path = "upstream_http_retire.rs"]
mod retire;
#[path = "upstream_http_runtime.rs"]
mod runtime;
#[cfg(test)]
#[path = "upstream_http_size_tests.rs"]
mod size_tests;
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
const MAX_RESPONSE_BYTES: usize = 16 * 1024 * 1024;

pub(crate) fn configured_request_timeout(timeout_ms: Option<u64>) -> Duration {
    timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(REQUEST_TIMEOUT)
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub(super) struct HttpFailure {
    pub(super) metadata: TransportFailure,
    message: String,
}

impl HttpFailure {
    pub(super) fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            metadata: TransportFailure {
                kind,
                status_code: None,
            },
            message: message.into(),
        }
    }

    pub(super) fn status(status: u16, message: impl Into<String>) -> Self {
        let kind = if matches!(status, 401 | 403) {
            FailureKind::Auth
        } else {
            FailureKind::Http
        };
        Self {
            metadata: TransportFailure {
                kind,
                status_code: Some(status),
            },
            message: message.into(),
        }
    }
}

impl From<&str> for HttpFailure {
    fn from(message: &str) -> Self {
        Self::new(FailureKind::Error, message)
    }
}

impl From<String> for HttpFailure {
    fn from(message: String) -> Self {
        Self::new(FailureKind::Error, message)
    }
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
    DiscoveryUnsupported(u16),
}

#[cfg(test)]
pub async fn spawn(
    url: String,
    sse: bool,
    response_tx: mpsc::Sender<String>,
) -> io::Result<UpstreamHandle> {
    spawn_configured(
        url,
        sse,
        BTreeMap::new(),
        None,
        None,
        response_tx,
        ServerLog::default(),
    )
    .await
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
    log: ServerLog,
) -> io::Result<UpstreamHandle> {
    let options = Options::new(headers, timeout_ms, auth, log)?;
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
    let log = options.log.clone();
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
            options.log.log("upstream_http_transport_stopped");
        }
        if deliberate {
            retire::terminate_session(&client, &url, &session, &options).await;
        }
        drop(client);
        completion_tx.send_replace(Some(retirement));
    });
    Ok(UpstreamHandle::new(request_tx, shutdown_tx, completion).with_log(log))
}

fn release_initialization(
    established: &mut Option<oneshot::Sender<Option<Request>>>,
    log: &ServerLog,
) {
    if let Some(established) = established.take()
        && established.send(None).is_err()
    {
        log.log("upstream_http_initialization_waiter_closed");
    }
}

fn parse_message(body: &str) -> Result<Value, HttpFailure> {
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

async fn send_failure(
    response_tx: &mpsc::Sender<String>,
    identifier: Option<&Value>,
    error: &HttpFailure,
    log: &ServerLog,
) {
    if let Some(identifier) = identifier {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": identifier,
            "error": {"code": -32000, "message": error.to_string(), "data": error.metadata.data()}
        });
        if response_tx.send(payload.to_string()).await.is_err() {
            log.log("upstream_http_response_receiver_closed");
        }
    } else {
        log.log("upstream_http_notification_failed");
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
) -> Result<Option<Vec<u8>>, HttpFailure> {
    timeout(deadline, response.chunk())
        .await
        .map_err(|_| {
            HttpFailure::new(FailureKind::Timeout, "HTTP response read deadline exceeded")
        })?
        .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
        .map_err(|_| {
            HttpFailure::new(
                FailureKind::Offline,
                "HTTP response read failed; request was not replayed",
            )
        })
}

async fn read_chunk_with_deadline(
    response: &mut reqwest::Response,
    deadline: &crate::request_deadline::SharedDeadline,
) -> Result<Option<Vec<u8>>, HttpFailure> {
    deadline
        .wait(response.chunk())
        .await
        .map_err(|_| {
            HttpFailure::new(FailureKind::Timeout, "HTTP response read deadline exceeded")
        })?
        .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
        .map_err(|_| {
            HttpFailure::new(
                FailureKind::Offline,
                "HTTP response read failed; request was not replayed",
            )
        })
}
