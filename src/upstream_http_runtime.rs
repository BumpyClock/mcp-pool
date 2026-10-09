use super::*;

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
) -> Result<(), String> {
    let mut first_request = true;
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
                let request = match Request::parse(line.clone()).and_then(|mut request| {
                    request.establish_budget(options)?;
                    Ok(request)
                }) {
                    Ok(request) => request,
                    Err(error) => {
                        let value = serde_json::from_str::<Value>(&line).ok();
                        let identifier = value.as_ref().and_then(|value| value.get("id"));
                        send_error(&response_tx, identifier, &error).await;
                        continue;
                    }
                };
                let allow_fallback = first_request && request.initialize && workers.is_empty();
                first_request = false;
                if request.initialization_barrier {
                    // Initialization establishes the headers every later request must use.
                    if request.initialize {
                        let worker_client = client.clone();
                        let worker_url = url.clone();
                        let worker_responses = response_tx.clone();
                        let worker_session = session.clone();
                        let worker_options = options.clone();
                        let (established, establishment) = oneshot::channel();
                        workers.spawn(async move {
                            execute(
                                &worker_client, &worker_url, request, &worker_responses, &worker_session, Some(established), &worker_options, allow_fallback,
                            ).await;
                        });
                        if let Some(request) = establishment.await.map_err(|_| "HTTP initialization worker stopped")? {
                            // Only an explicit initial POST rejection permits sending initialize via legacy SSE.
                            let handle = match legacy::spawn(client.clone(), url.clone(), response_tx.clone(), options.clone(), request.budget.clone()).await {
                                Ok(handle) => handle,
                                Err(_) => {
                                    send_error(&response_tx, request.identifier.as_ref(), "Legacy SSE fallback setup failed; initialize was not replayed").await;
                                    return Err("Legacy SSE fallback setup failed".into());
                                }
                            };
                            let sender = handle.request_tx.clone();
                            *fallback = Some(handle);
                            sender.send(request.forwarded()?).await.map_err(|_| "Legacy SSE fallback stopped")?;
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
                        execute(&client, &url, request, &response_tx, &session, None, options, false).await;
                    }
                } else {
                    let client = client.clone();
                    let url = url.clone();
                    let response_tx = response_tx.clone();
                    let session = session.clone();
                    let options = options.clone();
                    workers.spawn(async move {
                        execute(&client, &url, request, &response_tx, &session, None, &options, false).await;
                    });
                }
            }
        }
    }
}
