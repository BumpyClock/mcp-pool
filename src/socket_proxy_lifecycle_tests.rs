use super::*;
use tokio::sync::oneshot;

static NEXT_SOCKET: AtomicU32 = AtomicU32::new(0);

pub(super) fn proxy() -> Arc<SocketProxy> {
    let identity = format!(
        "{}-{}",
        std::process::id(),
        NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
    );
    #[cfg(windows)]
    let socket = PathBuf::from(format!(r"\\.\pipe\mcp-pool-lifecycle-{identity}"));
    #[cfg(unix)]
    let socket = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!(".mcp-pool-lifecycle-{identity}.sock"));
    Arc::new(SocketProxy::new(
        identity,
        socket,
        UpstreamSpec::Stdio {
            command: "unused-test-backend".to_string(),
            args: Vec::new(),
            env: Default::default(),
        },
        true,
    ))
}

pub(super) struct Backend {
    pub(super) setup: oneshot::Sender<io::Result<UpstreamHandle>>,
    pub(super) handle: UpstreamHandle,
    pub(super) shutdown: oneshot::Receiver<()>,
    pub(super) retired: watch::Sender<Completion>,
    pub(super) responses: mpsc::Sender<String>,
    pub(super) requests: mpsc::Receiver<String>,
}

pub(super) fn backend(proxy: &SocketProxy) -> Backend {
    let (setup, setup_rx) = oneshot::channel();
    let (responses, response_rx) = mpsc::channel(16);
    *proxy.test_setup.lock() = Some((setup_rx, response_rx));
    let (requests_tx, requests) = mpsc::channel(16);
    let (shutdown_tx, shutdown) = oneshot::channel();
    let (retired, completion) = watch::channel(None);
    Backend {
        setup,
        handle: UpstreamHandle::new(requests_tx, shutdown_tx, completion),
        shutdown,
        retired,
        responses,
        requests,
    }
}

async fn starting(proxy: &SocketProxy) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        while proxy.status() != ServerStatus::Starting {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)
}

async fn finish(task: tokio::task::JoinHandle<io::Result<()>>) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(2), task)
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)?
}

#[tokio::test]
async fn concurrent_start_is_idempotent_and_readiness_is_genuine() -> io::Result<()> {
    let proxy = proxy();
    let Backend {
        setup,
        handle,
        mut shutdown,
        retired,
        responses,
        ..
    } = backend(&proxy);
    let first = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.start().await })
    };
    starting(&proxy).await?;
    let second = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.start().await })
    };
    assert!(proxy.readiness().local_socket_bound);
    assert!(!proxy.readiness().upstream_transport_ready);
    assert!(!proxy.readiness().mcp_initialize_result_received);
    assert!(!first.is_finished());
    assert!(!second.is_finished());
    setup
        .send(Ok(handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    finish(first).await?;
    finish(second).await?;
    assert_eq!(proxy.status(), ServerStatus::Running);
    assert!(proxy.readiness().upstream_transport_ready);
    assert!(!proxy.readiness().mcp_initialize_result_received);
    // Idempotent start must not drop the sole handle or request its retirement.
    assert!(shutdown.try_recv().is_err());
    let stop = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.stop().await })
    };
    shutdown.await.map_err(io::Error::other)?;
    assert!(!stop.is_finished());
    retired.send_replace(Some(Ok(())));
    finish(stop).await?;
    assert_eq!(proxy.status(), ServerStatus::Stopped);
    assert!(!proxy.readiness().local_socket_bound);
    drop(responses);
    Ok(())
}

#[tokio::test]
async fn stop_during_setup_retires_backend_before_completion() -> io::Result<()> {
    let proxy = proxy();
    let Backend {
        setup,
        handle,
        shutdown,
        retired,
        responses,
        ..
    } = backend(&proxy);
    let start = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.start().await })
    };
    starting(&proxy).await?;
    let stop = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.stop().await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while !proxy
            .generation
            .lock()
            .as_ref()
            .is_some_and(|generation| generation.explicit_stop.load(Ordering::SeqCst))
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    setup
        .send(Ok(handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    shutdown.await.map_err(io::Error::other)?;
    assert!(finish(start).await.is_err());
    assert!(!stop.is_finished());
    retired.send_replace(Some(Ok(())));
    finish(stop).await?;
    assert_eq!(proxy.status(), ServerStatus::Stopped);
    drop(responses);
    Ok(())
}

#[tokio::test]
async fn restart_waits_for_retirement_and_discards_generation_state() -> io::Result<()> {
    let proxy = proxy();
    let Backend {
        setup,
        handle,
        shutdown,
        retired,
        responses,
        ..
    } = backend(&proxy);
    setup
        .send(Ok(handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    proxy.start().await?;
    let previous = proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    previous
        .handshake_cache
        .lock()
        .store("initialize", serde_json::json!({"version":"old"}));
    previous.client_capabilities.lock().insert(
        "old".to_string(),
        ClientCapabilities {
            roots: true,
            sampling: true,
        },
    );
    *previous.last_active_client.lock() = Some("old".to_string());
    previous.cleanup_counter.store(99, Ordering::SeqCst);
    let Backend {
        setup: next_setup,
        handle: next_handle,
        shutdown: next_shutdown,
        retired: next_retired,
        responses: next_responses,
        ..
    } = backend(&proxy);
    let restart = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.restart().await })
    };
    shutdown.await.map_err(io::Error::other)?;
    assert!(!restart.is_finished());
    assert!(
        !previous.socket_bound.load(Ordering::SeqCst) || proxy.status() == ServerStatus::Stopping
    );
    retired.send_replace(Some(Ok(())));
    starting(&proxy).await?;
    next_setup
        .send(Ok(next_handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    assert!(restart.await.map_err(io::Error::other)??);
    let current = proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    assert!(!Arc::ptr_eq(&previous, &current));
    assert!(current.handshake_cache.lock().get("initialize").is_none());
    assert!(current.request_map.lock().is_empty());
    assert!(current.client_capabilities.lock().is_empty());
    assert!(current.last_active_client.lock().is_none());
    assert_eq!(current.cleanup_counter.load(Ordering::SeqCst), 0);
    let stop = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.stop().await })
    };
    next_shutdown.await.map_err(io::Error::other)?;
    next_retired.send_replace(Some(Ok(())));
    finish(stop).await?;
    drop((responses, next_responses));
    Ok(())
}

#[tokio::test]
async fn retirement_error_prohibits_start_and_restart() -> io::Result<()> {
    let proxy = proxy();
    let Backend {
        setup,
        handle,
        shutdown,
        retired,
        responses,
        ..
    } = backend(&proxy);
    setup
        .send(Ok(handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    proxy.start().await?;
    let stop = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.stop().await })
    };
    shutdown.await.map_err(io::Error::other)?;
    retired.send_replace(Some(Err("tree retirement unverified".to_string())));
    assert_eq!(
        finish(stop).await.err().map(|error| error.to_string()),
        Some("tree retirement unverified".to_string())
    );
    assert_eq!(proxy.status(), ServerStatus::Failed);
    assert!(proxy.start().await.is_err());
    assert!(proxy.restart().await.is_err());
    assert_eq!(
        proxy.readiness().retirement_error.as_deref(),
        Some("tree retirement unverified")
    );
    drop(responses);
    Ok(())
}

#[tokio::test]
async fn startup_failure_is_reported_and_socket_is_retired() -> io::Result<()> {
    let proxy = proxy();
    let Backend {
        setup, responses, ..
    } = backend(&proxy);
    setup
        .send(Err(io::Error::other("setup rejected")))
        .map_err(|_| io::Error::other("setup lost"))?;
    assert_eq!(
        proxy.start().await.err().map(|error| error.to_string()),
        Some("setup rejected".to_string())
    );
    proxy.stop().await?;
    assert_eq!(
        proxy.readiness().startup_error.as_deref(),
        Some("setup rejected")
    );
    assert!(!proxy.readiness().local_socket_bound);
    assert!(!proxy.readiness().upstream_transport_ready);
    drop(responses);
    Ok(())
}

#[tokio::test]
async fn first_client_request_queues_through_startup() -> io::Result<()> {
    let proxy = proxy();
    let Backend {
        setup,
        handle,
        shutdown,
        retired,
        responses,
        mut requests,
    } = backend(&proxy);
    let start = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.start().await })
    };
    starting(&proxy).await?;
    let mut client = crate::transport::connect(&proxy.socket_path()).await?;
    client.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"capabilities\":{}}}\n").await?;
    setup
        .send(Ok(handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    finish(start).await?;
    let request = tokio::time::timeout(Duration::from_secs(2), requests.recv())
        .await
        .map_err(io::Error::other)?
        .ok_or_else(|| io::Error::other("request lost"))?;
    let request: Value = serde_json::from_str(&request)?;
    responses
        .send(
            serde_json::json!({
                "jsonrpc":"2.0", "id":request.get("id"), "result":{"capabilities":{}}
            })
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line))
        .await
        .map_err(io::Error::other)??;
    let response: Value = serde_json::from_str(&line)?;
    assert_eq!(response.get("id"), Some(&Value::from(1)));
    assert!(proxy.readiness().mcp_initialize_result_received);
    let stop = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.stop().await })
    };
    shutdown.await.map_err(io::Error::other)?;
    retired.send_replace(Some(Ok(())));
    finish(stop).await?;
    Ok(())
}

#[tokio::test]
async fn natural_completion_retires_local_tasks_and_clears_readiness() -> io::Result<()> {
    let proxy = proxy();
    let Backend {
        setup,
        handle,
        retired,
        responses,
        ..
    } = backend(&proxy);
    setup
        .send(Ok(handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    proxy.start().await?;
    retired.send_replace(Some(Ok(())));
    tokio::time::timeout(Duration::from_secs(2), async {
        while proxy.status() != ServerStatus::Stopped {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    assert!(!proxy.readiness().upstream_transport_ready);
    assert!(!proxy.readiness().local_socket_bound);
    assert_eq!(proxy.connection_count(), 0);
    proxy.stop().await?;
    drop(responses);
    Ok(())
}

#[tokio::test]
async fn unverified_startup_retirement_prohibits_replacement() -> io::Result<()> {
    let proxy = proxy();
    let Backend {
        setup, responses, ..
    } = backend(&proxy);
    setup
        .send(Err(io::Error::new(
            io::ErrorKind::ResourceBusy,
            "setup retirement unverified",
        )))
        .map_err(|_| io::Error::other("setup lost"))?;
    assert!(proxy.start().await.is_err());
    assert!(proxy.stop().await.is_err());
    assert_eq!(proxy.status(), ServerStatus::Failed);
    assert!(proxy.start().await.is_err());
    assert_eq!(
        proxy.readiness().retirement_error.as_deref(),
        Some("setup retirement unverified")
    );
    drop(responses);
    Ok(())
}

#[tokio::test]
async fn pool_concurrent_start_is_idempotent_and_stop_waits_for_retirement() -> io::Result<()> {
    let proxy = proxy();
    let Backend {
        setup,
        handle,
        shutdown,
        retired,
        responses,
        ..
    } = backend(&proxy);
    let start = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.start().await })
    };
    starting(&proxy).await?;
    let pool = Arc::new(crate::pool::Pool::new());
    pool.insert_test_proxy("test-server", proxy.clone());
    let concurrent_start = {
        let pool = pool.clone();
        let spec = proxy.spec.clone();
        tokio::spawn(async move { pool.start("test-server", spec).await })
    };
    setup
        .send(Ok(handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    finish(start).await?;
    finish(concurrent_start).await?;
    assert_eq!(pool.get_status().server_count, 1);
    let stop = {
        let pool = pool.clone();
        tokio::spawn(async move { pool.stop_server("test-server").await })
    };
    shutdown.await.map_err(io::Error::other)?;
    assert!(!stop.is_finished());
    retired.send_replace(Some(Ok(())));
    assert!(stop.await.map_err(io::Error::other)??);
    assert_eq!(pool.get_status().server_count, 0);
    drop(responses);
    Ok(())
}

#[tokio::test]
async fn stop_before_generation_publication_does_not_launch_backend() -> io::Result<()> {
    let proxy = proxy();
    let Backend {
        setup,
        handle,
        shutdown,
        retired,
        responses,
        ..
    } = backend(&proxy);
    proxy.request_stop();
    assert!(proxy.start().await.is_err());
    assert!(proxy.generation.lock().is_none());
    proxy.stop().await?;
    setup
        .send(Ok(handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    proxy.start().await?;
    let stop = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.stop().await })
    };
    shutdown.await.map_err(io::Error::other)?;
    retired.send_replace(Some(Ok(())));
    finish(stop).await?;
    drop(responses);
    Ok(())
}
