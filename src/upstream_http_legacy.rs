use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

struct Pending {
    identifier: Value,
    initialize: bool,
    answered: Arc<AtomicBool>,
    done: oneshot::Sender<Result<(), String>>,
}

type Requests = Arc<Mutex<HashMap<String, Pending>>>;

pub(super) async fn spawn(
    client: reqwest::Client,
    url: reqwest::Url,
    response_tx: mpsc::Sender<String>,
    options: Options,
    startup_budget: Option<crate::request_deadline::SharedDeadline>,
) -> io::Result<UpstreamHandle> {
    let discovery_budget = match &startup_budget {
        Some(budget) => budget.clone(),
        None => crate::request_deadline::SharedDeadline::new(
            std::time::Instant::now()
                .checked_add(options.request_timeout)
                .ok_or_else(|| {
                    io::Error::other("Legacy SSE discovery deadline exceeds the clock range")
                })?,
        ),
    };
    let (mut response, mut decoder, endpoint) = discovery_budget
        .wait(async {
            let mut response = options
                .authorize(client.get(url.clone()))
                .await?
                .headers(HeaderMap::from_iter([(
                    ACCEPT,
                    HeaderValue::from_static("text/event-stream"),
                )]))
                .send()
                .await
                .map_err(|_| "Legacy SSE connection failed")?;
            if !response.status().is_success() || content_type(&response) != "text/event-stream" {
                return Err("Legacy SSE requires a successful GET event stream".into());
            }
            let mut decoder = sse_parser::Decoder::new();
            loop {
                let chunk = match &startup_budget {
                    Some(budget) => read_chunk_with_deadline(&mut response, budget).await,
                    None => read_chunk_with_timeout(&mut response, options.read_timeout).await,
                }
                .map_err(|_| "Legacy SSE endpoint discovery read failed")?
                .ok_or("Legacy SSE ended before endpoint discovery")?;
                let feed = decoder.feed(&chunk);
                let mut endpoint = None;
                for event in feed.events {
                    if event.name == "endpoint" {
                        let candidate = url
                            .join(&event.data)
                            .map_err(|_| "Legacy SSE advertised an invalid message endpoint")?;
                        // The discovery stream cannot redirect credentials or messages to another origin.
                        if candidate.origin() != url.origin()
                            || candidate.username() != url.username()
                            || candidate.password() != url.password()
                            || candidate.fragment().is_some()
                        {
                            return Err(
                                "Legacy SSE message endpoint must use the same origin".into()
                            );
                        }
                        endpoint = Some(candidate);
                    } else if event.name == "message" || event.name.is_empty() {
                        let message = parse_message(&event.data)
                            .map_err(|_| "Legacy SSE discovery returned an invalid message")?;
                        response_tx
                            .send(message.to_string())
                            .await
                            .map_err(|_| "Legacy SSE response receiver closed")?;
                    }
                }
                if feed.error.is_some() {
                    return Err("Legacy SSE endpoint discovery frame is invalid".into());
                }
                if let Some(endpoint) = endpoint {
                    return Ok::<_, String>((response, decoder, endpoint));
                }
            }
        })
        .await
        .map_err(|_| io::Error::other("Legacy SSE endpoint discovery deadline exceeded"))?
        .map_err(io::Error::other)?;

    let (request_tx, request_rx) = mpsc::channel(1024);
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (completion_tx, completion) = watch::channel(None);
    tokio::spawn(async move {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let session = Arc::new(Mutex::new(Session::Fresh));
        let mut workers = JoinSet::new();
        let result = {
            let stream = receive(
                &mut response,
                &mut decoder,
                &pending,
                &session,
                &response_tx,
            );
            let requests = run_requests(
                &client,
                &endpoint,
                request_rx,
                &pending,
                &session,
                &response_tx,
                &mut workers,
                &options,
            );
            tokio::pin!(stream);
            tokio::pin!(requests);
            tokio::select! {
                _ = shutdown_rx => Ok(()),
                result = &mut stream => result,
                result = &mut requests => result,
            }
        };
        workers.abort_all();
        while workers.join_next().await.is_some() {}
        pending.lock().await.clear();
        drop(response);
        drop(client);
        if result.is_err() {
            crate::diagnostics::log("upstream_http_legacy_transport_stopped");
        }
        completion_tx.send_replace(Some(Ok(())));
    });
    Ok(UpstreamHandle::new(request_tx, shutdown_tx, completion))
}

async fn receive(
    response: &mut reqwest::Response,
    decoder: &mut sse_parser::Decoder,
    pending: &Requests,
    session: &Arc<Mutex<Session>>,
    response_tx: &mpsc::Sender<String>,
) -> Result<(), String> {
    let result = receive_events(response, decoder, pending, session, response_tx).await;
    if result.is_err() {
        let requests: Vec<Pending> = pending
            .lock()
            .await
            .drain()
            .map(|(_, request)| request)
            .collect();
        for request in requests {
            send_error(
                response_tx,
                Some(&request.identifier),
                "Legacy SSE connection failed; request was not replayed",
            )
            .await;
            request.answered.store(true, Ordering::Release);
        }
    }
    result
}

async fn receive_events(
    response: &mut reqwest::Response,
    decoder: &mut sse_parser::Decoder,
    pending: &Requests,
    session: &Arc<Mutex<Session>>,
    response_tx: &mpsc::Sender<String>,
) -> Result<(), String> {
    loop {
        // Idle time does not retire a persistent pool; execute bounds each pending request.
        let chunk = response
            .chunk()
            .await
            .map_err(|_| "Legacy SSE connection read failed; request was not replayed")?
            .ok_or("Legacy SSE connection ended; restart the upstream")?;
        let feed = decoder.feed(&chunk);
        for event in feed.events {
            if !event.name.is_empty() && event.name != "message" {
                continue;
            }
            let value = parse_message(&event.data)?;
            let key = value
                .get("id")
                .filter(|_| value.get("result").is_some() || value.get("error").is_some())
                .map(Value::to_string);
            let request = match key {
                Some(key) => pending.lock().await.remove(&key),
                None => None,
            };
            if let Some(request) = request {
                let result = if request.initialize && value.get("result").is_some() {
                    match value
                        .pointer("/result/protocolVersion")
                        .and_then(Value::as_str)
                    {
                        Some(protocol) if !protocol.is_empty() => {
                            match HeaderValue::from_str(protocol) {
                                Ok(protocol) => {
                                    *session.lock().await = Session::Ready {
                                        identifier: None,
                                        protocol,
                                    };
                                    Ok(())
                                }
                                Err(_) => {
                                    Err("Legacy SSE initialize protocolVersion is invalid".into())
                                }
                            }
                        }
                        _ => Err("Legacy SSE initialize omitted a valid protocolVersion".into()),
                    }
                } else {
                    Ok(())
                };
                if result.is_ok() {
                    response_tx
                        .send(value.to_string())
                        .await
                        .map_err(|_| "Legacy SSE response receiver closed")?;
                    request.answered.store(true, Ordering::Release);
                }
                if request.done.send(result).is_err() {
                    crate::diagnostics::log("upstream_http_legacy_request_waiter_closed");
                }
            } else {
                response_tx
                    .send(value.to_string())
                    .await
                    .map_err(|_| "Legacy SSE response receiver closed")?;
            }
        }
        if let Some(error) = feed.error {
            return Err(error);
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_requests(
    client: &reqwest::Client,
    endpoint: &reqwest::Url,
    mut request_rx: mpsc::Receiver<crate::upstream::UpstreamRequest>,
    pending: &Requests,
    session: &Arc<Mutex<Session>>,
    response_tx: &mpsc::Sender<String>,
    workers: &mut JoinSet<()>,
    options: &Options,
) -> Result<(), String> {
    loop {
        tokio::select! {
            biased;
            _ = response_tx.closed() => return Ok(()),
            result = workers.join_next(), if !workers.is_empty() => {
                if result.is_some_and(|result| result.is_err()) {
                    return Err("Legacy SSE request worker failed".into());
                }
            }
            line = request_rx.recv(), if workers.len() < MAX_CONCURRENT_REQUESTS => {
                let Some(line) = line else {
                    while workers.join_next().await.is_some() {}
                    return Ok(());
                };
                let request = match Request::parse(line.clone()).and_then(|mut request| {
                    request.establish_budget(options)?;
                    Ok(request)
                }) {
                    Ok(request) => request,
                    Err(error) => {
                        let value = serde_json::from_str::<Value>(&line).ok();
                        let identifier = value.as_ref().and_then(|value| value.get("id"));
                        send_error(response_tx, identifier, &error).await;
                        continue;
                    }
                };
                if request.initialization_barrier {
                    while workers.join_next().await.is_some() {}
                    execute(client, endpoint, request, pending, session, response_tx, options).await;
                } else {
                    let client = client.clone();
                    let endpoint = endpoint.clone();
                    let pending = pending.clone();
                    let session = session.clone();
                    let response_tx = response_tx.clone();
                    let options = options.clone();
                    workers.spawn(async move {
                        execute(&client, &endpoint, request, &pending, &session, &response_tx, &options).await;
                    });
                }
            }
        }
    }
}

async fn execute(
    client: &reqwest::Client,
    endpoint: &reqwest::Url,
    request: Request,
    pending: &Requests,
    session: &Arc<Mutex<Session>>,
    response_tx: &mpsc::Sender<String>,
    options: &Options,
) {
    let key = request.identifier.as_ref().map(Value::to_string);
    let answered = Arc::new(AtomicBool::new(false));
    let Some(budget) = &request.budget else {
        send_error(
            response_tx,
            request.identifier.as_ref(),
            "Legacy SSE request budget missing",
        )
        .await;
        return;
    };
    let result = budget
        .wait(async {
            let state = session.lock().await.clone();
            if matches!(state, Session::Expired) {
                return Err(
                    "Legacy SSE session expired; restart the upstream before sending requests"
                        .into(),
                );
            }
            if matches!(state, Session::Ready { .. }) && request.initialize {
                return Err("Legacy SSE upstream is already initialized".into());
            }
            let (done, completed) = oneshot::channel();
            if let (Some(key), Some(identifier)) = (&key, &request.identifier) {
                pending.lock().await.insert(
                    key.clone(),
                    Pending {
                        identifier: identifier.clone(),
                        initialize: request.initialize,
                        answered: answered.clone(),
                        done,
                    },
                );
            }
            let mut builder = options
                .authorize(client.post(endpoint.clone()))
                .await?
                .headers(HeaderMap::from_iter([
                    (CONTENT_TYPE, HeaderValue::from_static("application/json")),
                    (
                        ACCEPT,
                        HeaderValue::from_static("application/json, text/event-stream"),
                    ),
                ]));
            if let Session::Ready { protocol, .. } = state {
                builder = builder.header("MCP-Protocol-Version", protocol);
            }
            let response = builder
                .body(request.line.clone())
                .send()
                .await
                .map_err(|_| "Legacy SSE POST failed; request was not replayed")?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                *session.lock().await = Session::Expired;
                return Err(
                    "Legacy SSE session expired (HTTP 404); request was not replayed".into(),
                );
            }
            if !response.status().is_success() {
                return Err(format!(
                    "Legacy SSE POST returned status {}; request was not replayed",
                    response.status().as_u16()
                ));
            }
            if key.is_some() {
                completed
                    .await
                    .map_err(|_| "Legacy SSE response stream closed")?
            } else {
                Ok(())
            }
        })
        .await
        .unwrap_or_else(|_| {
            Err("Legacy SSE request deadline exceeded; request was not replayed".into())
        });
    if let Some(key) = key {
        pending.lock().await.remove(&key);
    }
    if let Err(error) = result
        && !answered.load(Ordering::Acquire)
    {
        send_error(response_tx, request.identifier.as_ref(), &error).await;
    }
}
