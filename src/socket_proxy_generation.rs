use super::*;

pub(super) struct Generation {
    pub request_tx: Arc<Mutex<Option<mpsc::Sender<String>>>>,
    pub clients: Arc<Mutex<HashMap<String, ClientSender>>>,
    pub request_map: RequestMap,
    pub handshake_cache: HandshakeCacheRef,
    pub client_capabilities: Arc<Mutex<HashMap<String, ClientCapabilities>>>,
    pub last_active_client: Arc<Mutex<Option<String>>>,
    pub id_allocator: Arc<IdAllocator>,
    pub shutdown: Arc<AtomicBool>,
    pub explicit_stop: AtomicBool,
    pub shutdown_notify: Arc<Notify>,
    pub upstream_ready: Arc<Notify>,
    pub cleanup_counter: Arc<AtomicU32>,
    pub startup: watch::Receiver<Completion>,
    startup_tx: watch::Sender<Completion>,
    pub completion: watch::Receiver<Completion>,
    completion_tx: watch::Sender<Completion>,
    pub socket_bound: AtomicBool,
}

impl Generation {
    pub fn new() -> Self {
        let (startup_tx, startup) = watch::channel(None);
        let (completion_tx, completion) = watch::channel(None);
        Self {
            request_tx: Arc::new(Mutex::new(None)),
            clients: Arc::new(Mutex::new(HashMap::new())),
            request_map: Arc::new(Mutex::new(HashMap::new())),
            handshake_cache: Arc::new(Mutex::new(HandshakeCache::default())),
            client_capabilities: Arc::new(Mutex::new(HashMap::new())),
            last_active_client: Arc::new(Mutex::new(None)),
            id_allocator: Arc::new(IdAllocator::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            explicit_stop: AtomicBool::new(false),
            shutdown_notify: Arc::new(Notify::new()),
            upstream_ready: Arc::new(Notify::new()),
            cleanup_counter: Arc::new(AtomicU32::new(0)),
            startup,
            startup_tx,
            completion,
            completion_tx,
            socket_bound: AtomicBool::new(true),
        }
    }

    pub fn signal_shutdown(&self) {
        self.explicit_stop.store(true, Ordering::SeqCst);
        self.close();
    }

    fn close(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_waiters();
        self.upstream_ready.notify_waiters();
    }

    fn clear(&self) {
        self.request_tx.lock().take();
        self.clients.lock().clear();
        self.request_map.lock().clear();
        *self.handshake_cache.lock() = HandshakeCache::default();
        self.client_capabilities.lock().clear();
        *self.last_active_client.lock() = None;
        self.cleanup_counter.store(0, Ordering::SeqCst);
    }

    async fn route(
        &self,
        message: &str,
        recovery_tx: &mpsc::Sender<RecoveryReason>,
        recovery_requested: &Arc<AtomicBool>,
    ) {
        route_response(
            message,
            &self.clients,
            &self.request_map,
            &self.handshake_cache,
            &self.cleanup_counter,
            &self.last_active_client,
            &self.client_capabilities,
            &self.request_tx,
            recovery_tx,
            recovery_requested,
        )
        .await;
    }
}

pub(super) async fn wait_completion(mut completion: watch::Receiver<Completion>) -> io::Result<()> {
    loop {
        if let Some(result) = completion.borrow().clone() {
            return result.map_err(io::Error::other);
        }
        completion.changed().await.map_err(|_| {
            io::Error::other("pool lifecycle owner exited without confirming retirement")
        })?;
    }
}

pub(super) fn spawn_owner(
    proxy: &Arc<SocketProxy>,
    generation: Arc<Generation>,
    listener: Arc<LocalListener>,
) {
    let spec = proxy.spec.clone();
    #[cfg(test)]
    if let Some((setup, response_rx)) = proxy.test_setup.lock().take() {
        spawn_owner_with(proxy, generation, listener, response_rx, async move {
            setup.await.map_err(io::Error::other)?
        });
        return;
    }
    let (response_tx, response_rx) = mpsc::channel(1024);
    spawn_owner_with(proxy, generation, listener, response_rx, async move {
        UpstreamHandle::spawn(spec, response_tx).await
    });
}

#[cfg(test)]
pub(super) type TestSetup = (
    tokio::sync::oneshot::Receiver<io::Result<UpstreamHandle>>,
    mpsc::Receiver<String>,
);

fn spawn_owner_with(
    proxy: &Arc<SocketProxy>,
    generation: Arc<Generation>,
    listener: Arc<LocalListener>,
    mut response_rx: mpsc::Receiver<String>,
    setup: impl std::future::Future<Output = io::Result<UpstreamHandle>> + Send + 'static,
) {
    let status = proxy.status.clone();
    let started_at = proxy.started_at.clone();
    let name = proxy.name.clone();
    let socket_path = proxy.socket_path.clone();
    let weak_proxy = Arc::downgrade(proxy);
    tokio::spawn(async move {
        let accept = tokio::spawn(accept_loop(listener, generation.clone(), name.clone()));
        let backend_generation = generation.clone();
        let backend_status = status.clone();
        let backend_started_at = started_at.clone();
        let backend_name = name.clone();
        let owner = tokio::spawn(async move {
            let generation = backend_generation;
            let status = backend_status;
            let started_at = backend_started_at;
            let name = backend_name;
            let (recovery_tx, mut recovery_rx) = mpsc::channel(8);
            let recovery_requested = Arc::new(AtomicBool::new(false));
            // Never cancel spawn: it can already own a child that must be retired.
            let spawned = setup.await;
            let mut recover = false;
            let retirement = match spawned {
                Ok(mut handle) => {
                    if generation.shutdown.load(Ordering::SeqCst) {
                        generation.startup_tx.send_replace(Some(Err(
                            "pool stopped during upstream startup".to_string(),
                        )));
                    } else {
                        *generation.request_tx.lock() = Some(handle.request_tx.clone());
                        *status.lock() = ServerStatus::Running;
                        *started_at.lock() = Some(Instant::now());
                        generation.startup_tx.send_replace(Some(Ok(())));
                        generation.upstream_ready.notify_waiters();
                    }
                    while !generation.shutdown.load(Ordering::SeqCst) {
                        let shutdown = generation.shutdown_notify.notified();
                        tokio::pin!(shutdown);
                        shutdown.as_mut().enable();
                        if generation.shutdown.load(Ordering::SeqCst) {
                            break;
                        }
                        tokio::select! {
                            _ = &mut shutdown => break,
                            result = handle.wait_for_exit() => {
                                if let Err(error) = result {
                                    diagnostics::log(format!(
                                        "pool_upstream_exit_failed name={name} error={error}"
                                    ));
                                }
                                *status.lock() = ServerStatus::Stopping;
                                generation.request_tx.lock().take();
                                // Terminal completion can precede routing buffered final responses.
                                tokio::select! {
                                    _ = &mut shutdown => {}
                                    drained = tokio::time::timeout(Duration::from_millis(250), async {
                                        while let Ok(message) = response_rx.try_recv() {
                                            generation.route(&message, &recovery_tx, &recovery_requested).await;
                                        }
                                    }) => {
                                        if drained.is_err() {
                                            diagnostics::log(format!("pool_terminal_response_drain_timed_out name={name}"));
                                        }
                                    }
                                }
                                recover = recovery_rx.try_recv().is_ok();
                                break;
                            }
                            reason = recovery_rx.recv() => {
                                if let Some(reason) = reason {
                                    diagnostics::log(format!(
                                        "pool_recovery_start name={name} reason={}",
                                        recovery_reason_label(reason)
                                    ));
                                    recover = true;
                                }
                                break;
                            }
                            message = response_rx.recv() => {
                                let Some(message) = message else { break };
                                // Slow client channels must not delay backend retirement.
                                tokio::select! {
                                    _ = &mut shutdown => break,
                                    _ = generation.route(&message, &recovery_tx, &recovery_requested) => {}
                                }
                            }
                        }
                    }
                    generation.close();
                    *status.lock() = ServerStatus::Stopping;
                    handle.shutdown().await.map_err(|error| error.to_string())
                }
                Err(error) => {
                    diagnostics::log(format!(
                        "pool_upstream_spawn_failed name={name} error={error}"
                    ));
                    generation
                        .startup_tx
                        .send_replace(Some(Err(error.to_string())));
                    generation.close();
                    // ResourceBusy explicitly means partial setup could not be retired.
                    if error.kind() == io::ErrorKind::ResourceBusy {
                        Err(error.to_string())
                    } else {
                        Ok(())
                    }
                }
            };
            (retirement, recover)
        });
        let (retirement, recover) = match owner.await {
            Ok(outcome) => outcome,
            Err(error) => {
                let error = format!("upstream lifecycle owner failed: {error}");
                if generation.startup.borrow().is_none() {
                    generation.startup_tx.send_replace(Some(Err(error.clone())));
                }
                (Err(error), false)
            }
        };
        generation.close();
        let local_retirement = accept.await.map_err(|error| error.to_string());
        generation.socket_bound.store(false, Ordering::SeqCst);
        generation.clear();
        *started_at.lock() = None;
        let retirement = retirement.and(local_retirement);
        #[cfg(unix)]
        let retirement = match std::fs::remove_file(&socket_path) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                retirement.and(Err(error.to_string()))
            }
            _ => retirement,
        };
        #[cfg(windows)]
        drop(socket_path);
        *status.lock() = if retirement.is_ok() {
            ServerStatus::Stopped
        } else {
            ServerStatus::Failed
        };
        diagnostics::log(format!(
            "pool_upstream_retired name={name} verified={}",
            retirement.is_ok()
        ));
        generation
            .completion_tx
            .send_replace(Some(retirement.clone()));
        if recover
            && retirement.is_ok()
            && let Some(proxy) = weak_proxy.upgrade()
        {
            tokio::spawn(async move { proxy.recover(generation).await });
        }
    });
}

async fn accept_loop(listener: Arc<LocalListener>, generation: Arc<Generation>, name: String) {
    let mut tasks = tokio::task::JoinSet::new();
    let mut counter = 0u64;
    loop {
        let shutdown = generation.shutdown_notify.notified();
        tokio::pin!(shutdown);
        shutdown.as_mut().enable();
        if generation.shutdown.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            _ = &mut shutdown => break,
            result = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = result {
                    diagnostics::log(format!("pool_client_task_failed name={name} error={error}"));
                }
            }
            accepted = listener.accept() => match accepted {
                Ok(stream) => {
                    let client_id = format!("{name}-client-{counter}");
                    counter += 1;
                    let (sender, receiver) = mpsc::channel(128);
                    generation.clients.lock().insert(client_id.clone(), sender);
                    let state = generation.clone();
                    tasks.spawn(async move {
                        handle_client(
                            stream, client_id, state.request_tx.clone(),
                            state.upstream_ready.clone(), state.id_allocator.clone(),
                            state.request_map.clone(), state.handshake_cache.clone(),
                            state.client_capabilities.clone(), state.last_active_client.clone(),
                            state.clients.clone(), state.shutdown.clone(),
                            state.shutdown_notify.clone(), state.cleanup_counter.clone(), receiver,
                        ).await;
                    });
                }
                Err(error) => {
                    diagnostics::log(format!("pool_accept_error name={name} error={error}"));
                    tokio::select! {
                        _ = &mut shutdown => break,
                        _ = sleep(Duration::from_millis(50)) => {}
                    }
                }
            }
        }
    }
    // Flush queued terminal errors, but do not let slow clients block retirement.
    if tokio::time::timeout(
        Duration::from_millis(250),
        drain_client_tasks(&mut tasks, &name),
    )
    .await
    .is_err()
    {
        diagnostics::log(format!(
            "pool_client_drain_timed_out name={name} pending_clients={}",
            tasks.len()
        ));
        tasks.abort_all();
        drain_client_tasks(&mut tasks, &name).await;
    }
}

async fn drain_client_tasks(tasks: &mut tokio::task::JoinSet<()>, name: &str) {
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            diagnostics::log(format!("pool_client_task_failed name={name} error={error}"));
        }
    }
}
