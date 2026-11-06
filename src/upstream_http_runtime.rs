use super::*;

/// Initialization establishes later request headers; only an explicit initial
/// POST rejection permits fallback to legacy SSE.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    client: reqwest::Client,
    url: reqwest::Url,
    mut request_rx: mpsc::Receiver<crate::upstream::UpstreamRequest>,
    response_tx: mpsc::Sender<String>,
    session: Arc<Mutex<Session>>,
    workers: &mut JoinSet<()>,
    options: &Options,
    fallback: &mut Option<UpstreamHandle>,
) -> Result<(), HttpFailure> {
    let mut negotiation = Negotiation::default();
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
                let request = match Request::parse(line, options) {
                    Ok(request) => request,
                    Err((line, error)) => {
                        let value = serde_json::from_str::<Value>(&line).ok();
                        let identifier = value.as_ref().and_then(|value| value.get("id"));
                        send_failure(&response_tx, identifier, &error, &options.log).await;
                        continue;
                    }
                };
                let mode = negotiation.admit(request.kind, workers.is_empty());
                let established_modern_refresh = if request.kind.is_discovery() && request.kind.is_modern() {
                    matches!(
                        &*session.lock().await,
                        Session::Ready { identifier: None, protocol }
                            if protocol == crate::mcp_protocol::MODERN_VERSION
                    )
                } else {
                    false
                };
                if request.kind.is_barrier() && !established_modern_refresh {
                    if request.kind.is_handshake() {
                        let worker_client = client.clone();
                        let worker_url = url.clone();
                        let worker_responses = response_tx.clone();
                        let worker_session = session.clone();
                        let worker_options = options.clone();
                        let (established, establishment) = oneshot::channel();
                        workers.spawn(async move {
                            execute(
                                &worker_client, &worker_url, request, &worker_responses, &worker_session, Some(established), &worker_options, mode,
                            ).await;
                        });
                        if let Some(request) = establishment.await.map_err(|_| "HTTP initialization worker stopped")? {
                            let identifier = request.identifier.clone();
                            let handle = match legacy::spawn(client.clone(), url.clone(), response_tx.clone(), options.clone(), Some(request)).await {
                                Ok(handle) => handle,
                                Err(_) => {
                                    send_failure(&response_tx, identifier.as_ref(), &HttpFailure::from("Legacy SSE fallback setup failed; initialize was not replayed"), &options.log).await;
                                    return Err("Legacy SSE fallback setup failed".into());
                                }
                            };
                            let sender = handle.request_tx.clone();
                            *fallback = Some(handle);
                            let handle = fallback.as_mut().ok_or("Legacy SSE fallback owner missing")?;
                            loop {
                                tokio::select! {
                                    _ = response_tx.closed() => return Ok(()),
                                    result = handle.wait_for_exit() => return result.map_err(|_| "Legacy SSE fallback retirement failed".into()),
                                    line = request_rx.recv() => match line {
                                        Some(line) => sender.send(line).await.map_err(|_| "Legacy SSE fallback stopped")?,
                                        None => return Ok(()),
                                    }
                                }
                            }
                        }
                    } else {
                        execute(&client, &url, request, &response_tx, &session, None, options, ExchangeMode::Standard).await;
                    }
                } else {
                    let client = client.clone();
                    let url = url.clone();
                    let response_tx = response_tx.clone();
                    let session = session.clone();
                    let options = options.clone();
                    workers.spawn(async move {
                        execute(&client, &url, request, &response_tx, &session, None, &options, ExchangeMode::Standard).await;
                    });
                }
            }
        }
    }
}
