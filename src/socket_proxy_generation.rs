use super::*;

pub(super) struct Generation {
    pub request_tx: Mutex<Option<mpsc::Sender<crate::upstream::UpstreamRequest>>>,
    pub clients: Mutex<HashMap<String, ClientSender>>,
    pub routes: SessionRoutes,
    pub diagnostics: diagnostics::ServerLog,
    pub client_capabilities: Mutex<HashMap<String, ClientCapabilities>>,
    pub last_active_client: Mutex<Option<String>>,
    pub shutdown: AtomicBool,
    pub explicit_stop: AtomicBool,
    pub shutdown_notify: Notify,
    pub upstream_ready: Notify,
    pub expiration_changed: Notify,
    pub startup: watch::Receiver<Completion>,
    startup_tx: watch::Sender<Completion>,
    pub completion: watch::Receiver<Completion>,
    completion_tx: watch::Sender<Completion>,
    pub socket_bound: AtomicBool,
}

impl Generation {
    pub fn new(diagnostics: diagnostics::ServerLog) -> Self {
        let (startup_tx, startup) = watch::channel(None);
        let (completion_tx, completion) = watch::channel(None);
        Self {
            request_tx: Mutex::new(None),
            clients: Mutex::new(HashMap::new()),
            routes: SessionRoutes::new(diagnostics.clone()),
            diagnostics,
            client_capabilities: Mutex::new(HashMap::new()),
            last_active_client: Mutex::new(None),
            shutdown: AtomicBool::new(false),
            explicit_stop: AtomicBool::new(false),
            shutdown_notify: Notify::new(),
            upstream_ready: Notify::new(),
            expiration_changed: Notify::new(),
            startup,
            startup_tx,
            completion,
            completion_tx,
            socket_bound: AtomicBool::new(false),
        }
    }

    pub fn signal_shutdown(&self) {
        self.explicit_stop.store(true, Ordering::SeqCst);
        self.close();
    }

    /// Confirms retirement without unlinking a socket this generation failed to bind.
    pub fn fail_binding(&self, error: &io::Error) {
        self.startup_tx.send_replace(Some(Err(error.to_string())));
        self.close();
        self.completion_tx.send_replace(Some(Ok(())));
    }

    fn close(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_waiters();
        self.upstream_ready.notify_waiters();
    }

    fn clear(&self) {
        self.request_tx.lock().take();
        self.clients.lock().clear();
        self.routes.clear();
        self.client_capabilities.lock().clear();
        *self.last_active_client.lock() = None;
    }

    async fn route(&self, message: &str, recovery_tx: &mpsc::Sender<RecoveryReason>) {
        route_response(message, self, recovery_tx).await;
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
    let log = proxy.diagnostics.clone();
    #[cfg(test)]
    if let Some((setup, response_rx)) = proxy.test_setup.lock().take() {
        spawn_owner_with(proxy, generation, listener, response_rx, async move {
            setup
                .await
                .map_err(io::Error::other)
                .map_err(crate::upstream::SetupFailure::from)?
        });
        return;
    }
    let (response_tx, response_rx) = mpsc::channel(1024);
    spawn_owner_with(proxy, generation, listener, response_rx, async move {
        UpstreamHandle::spawn(spec, response_tx, log).await
    });
}

#[cfg(test)]
pub(super) type TestSetup = (
    tokio::sync::oneshot::Receiver<Result<UpstreamHandle, crate::upstream::SetupFailure>>,
    mpsc::Receiver<String>,
);

/// Owns setup through retirement, including children created before shutdown.
/// Unverified startup retirement prohibits replacement; shutdown can cancel client delivery.
fn spawn_owner_with(
    proxy: &Arc<SocketProxy>,
    generation: Arc<Generation>,
    listener: Arc<LocalListener>,
    mut response_rx: mpsc::Receiver<String>,
    setup: impl std::future::Future<Output = Result<UpstreamHandle, crate::upstream::SetupFailure>>
    + Send
    + 'static,
) {
    let log = proxy.diagnostics.clone();
    let status = proxy.status.clone();
    let started_at = proxy.started_at.clone();
    let name = proxy.name.clone();
    let socket_path = proxy.socket_path.clone();
    let (remote, shared_timeout) = match &proxy.spec {
        UpstreamSpec::Stdio { .. } => (false, Duration::from_secs(REQUEST_TTL_SECS)),
        UpstreamSpec::Http { timeout_ms, .. } => (
            true,
            crate::upstream_http::configured_request_timeout(*timeout_ms),
        ),
    };
    let weak_proxy = Arc::downgrade(proxy);
    tokio::spawn(async move {
        let accept = tokio::spawn(accept_loop(
            listener,
            generation.clone(),
            name.clone(),
            remote,
            shared_timeout,
        ));
        let expiration = tokio::spawn(expiration_loop(generation.clone()));
        let backend_generation = generation.clone();
        let backend_status = status.clone();
        let backend_started_at = started_at.clone();
        let backend_name = name.clone();
        let backend_log = log.clone();
        let owner = tokio::spawn(async move {
            let log = backend_log;
            let generation = backend_generation;
            let status = backend_status;
            let started_at = backend_started_at;
            let name = backend_name;
            let (recovery_tx, mut recovery_rx) = mpsc::channel(8);
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
                                    log.log(format!(
                                        "pool_upstream_exit_failed name={name} error={error}"
                                    ));
                                }
                                *status.lock() = ServerStatus::Stopping;
                                generation.request_tx.lock().take();
                                tokio::select! {
                                    _ = &mut shutdown => {}
                                    drained = tokio::time::timeout(Duration::from_millis(250), async {
                                        while let Ok(message) = response_rx.try_recv() {
                                            generation.route(&message, &recovery_tx).await;
                                        }
                                    }) => {
                                        if drained.is_err() {
                                            log.log(format!("pool_terminal_response_drain_timed_out name={name}"));
                                        }
                                    }
                                }
                                recover = recovery_rx.try_recv().is_ok();
                                break;
                            }
                            reason = recovery_rx.recv() => {
                                if let Some(reason) = reason {
                                    log.log(format!(
                                        "pool_recovery_start name={name} reason={}",
                                        recovery_reason_label(reason)
                                    ));
                                    recover = true;
                                }
                                break;
                            }
                            message = response_rx.recv() => {
                                let Some(message) = message else { break };
                                tokio::select! {
                                    _ = &mut shutdown => break,
                                    _ = generation.route(&message, &recovery_tx) => {}
                                }
                            }
                        }
                    }
                    generation.close();
                    *status.lock() = ServerStatus::Stopping;
                    handle.shutdown().await.map_err(|error| error.to_string())
                }
                Err(error) => {
                    log.log(format!(
                        "pool_upstream_spawn_failed name={name} error={error}"
                    ));
                    generation
                        .startup_tx
                        .send_replace(Some(Err(error.to_string())));
                    generation.close();
                    if error.retirement_unverified() {
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
        let expiration_retirement = expiration.await.map_err(|error| error.to_string());
        let local_retirement = accept
            .await
            .map_err(|error| error.to_string())
            .and(expiration_retirement);
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
        log.log(format!(
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

async fn accept_loop(
    listener: Arc<LocalListener>,
    generation: Arc<Generation>,
    name: String,
    remote: bool,
    shared_timeout: Duration,
) {
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
                    generation.diagnostics.log(format!("pool_client_task_failed name={name} error={error}"));
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
                        handle_client(stream, client_id, state, remote, shared_timeout, receiver).await;
                    });
                }
                Err(error) => {
                    generation.diagnostics.log(format!("pool_accept_error name={name} error={error}"));
                    tokio::select! {
                        _ = &mut shutdown => break,
                        _ = sleep(Duration::from_millis(50)) => {}
                    }
                }
            }
        }
    }
    if tokio::time::timeout(
        Duration::from_millis(250),
        drain_client_tasks(&mut tasks, &name, &generation.diagnostics),
    )
    .await
    .is_err()
    {
        generation.diagnostics.log(format!(
            "pool_client_drain_timed_out name={name} pending_clients={}",
            tasks.len()
        ));
        tasks.abort_all();
        drain_client_tasks(&mut tasks, &name, &generation.diagnostics).await;
    }
}

/// Bounds timeout delivery so one stalled client cannot block later deadlines or shutdown.
pub(super) async fn expiration_loop(generation: Arc<Generation>) {
    loop {
        let shutdown = generation.shutdown_notify.notified();
        let changed = generation.expiration_changed.notified();
        tokio::pin!(shutdown, changed);
        shutdown.as_mut().enable();
        changed.as_mut().enable();
        if generation.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let deadline = generation.routes.next_deadline();
        let wait_deadline = async {
            match deadline {
                Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = &mut shutdown => break,
            _ = &mut changed => continue,
            _ = wait_deadline => {
                let responses = generation.routes.expire(Instant::now());
                for (client_id, payload) in sweep_recipients(responses, &generation.clients, &generation.diagnostics) {
                    tokio::select! {
                        _ = &mut shutdown => return,
                        result = tokio::time::timeout(
                            Duration::from_millis(250),
                            send_to_client(&client_id, payload, &generation.clients, &generation.diagnostics),
                        ) => {
                            if result.is_err() {
                                generation.diagnostics.log(format!("pool_timeout_delivery_failed client_id={client_id}"));
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn drain_client_tasks(
    tasks: &mut tokio::task::JoinSet<()>,
    name: &str,
    log: &diagnostics::ServerLog,
) {
    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result
            && !error.is_cancelled()
        {
            log.log(format!("pool_client_task_failed name={name} error={error}"));
        }
    }
}
