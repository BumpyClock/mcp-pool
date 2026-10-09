use super::lifecycle_tests::{Backend, backend, proxy};
use super::*;
use serde_json::json;
use tokio::io::AsyncReadExt;
use tokio::sync::oneshot;

pub(super) struct Fixture {
    pub(super) proxy: Arc<SocketProxy>,
    pub(super) responses: mpsc::Sender<String>,
    pub(super) requests: mpsc::Receiver<crate::upstream::UpstreamRequest>,
    shutdown: oneshot::Receiver<()>,
    pub(super) retired: watch::Sender<Completion>,
}

impl Fixture {
    pub(super) async fn start() -> io::Result<Self> {
        let proxy = proxy();
        let Backend {
            setup,
            handle,
            responses,
            requests,
            shutdown,
            retired,
        } = backend(&proxy);
        setup
            .send(Ok(handle))
            .map_err(|_| io::Error::other("setup lost"))?;
        proxy.start().await?;
        Ok(Self {
            proxy,
            responses,
            requests,
            shutdown,
            retired,
        })
    }

    pub(super) async fn request(&mut self) -> io::Result<Value> {
        let request = tokio::time::timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("request channel closed"))?;
        Ok(serde_json::from_str(&request)?)
    }

    pub(super) async fn stop(self) -> io::Result<()> {
        let proxy = self.proxy.clone();
        let stop = tokio::spawn(async move { proxy.stop().await });
        self.shutdown.await.map_err(io::Error::other)?;
        self.retired.send_replace(Some(Ok(())));
        stop.await.map_err(io::Error::other)??;
        Ok(())
    }
}

pub(super) async fn send(client: &mut LocalStream, value: Value) -> io::Result<()> {
    let mut payload = value.to_string();
    payload.push('\n');
    client.write_all(payload.as_bytes()).await
}

pub(super) async fn read(client: &mut BufReader<LocalStream>) -> io::Result<Value> {
    let mut line = String::new();
    let size = tokio::time::timeout(Duration::from_secs(2), client.read_line(&mut line))
        .await
        .map_err(io::Error::other)??;
    if size == 0 {
        return Err(io::Error::other("client disconnected before response"));
    }
    Ok(serde_json::from_str(&line)?)
}

pub(super) async fn follower_queued(proxy: &SocketProxy) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let state = proxy.generation.lock().clone();
            if let Some(state) = state
                && matches!(&state.handshake_cache.lock().initialize,
                    Initialization::InFlight { waiters, .. } if waiters.len() == 1)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)
}

pub(super) async fn connect(proxy: &SocketProxy) -> io::Result<LocalStream> {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match crate::transport::connect(&proxy.socket_path()).await {
                Err(error) if cfg!(windows) && error.raw_os_error() == Some(231) => {
                    sleep(Duration::from_millis(1)).await;
                }
                result => return result,
            }
        }
    })
    .await
    .map_err(io::Error::other)?
}

pub(super) async fn two_clients(
    fixture: &mut Fixture,
) -> io::Result<(BufReader<LocalStream>, BufReader<LocalStream>, Value)> {
    let mut first = connect(&fixture.proxy).await?;
    let mut second = connect(&fixture.proxy).await?;
    let (first_write, second_write) = tokio::join!(
        send(
            &mut first,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-03-26","capabilities":{"sampling":{}}}})
        ),
        send(
            &mut second,
            json!({"jsonrpc":"2.0","id":"second","method":"initialize",
            "params":{"protocolVersion":"2025-03-26","capabilities":{"roots":{}}}})
        ),
    );
    first_write?;
    second_write?;
    let leader = fixture.request().await?;
    follower_queued(&fixture.proxy).await?;
    assert_eq!(leader.get("method"), Some(&json!("initialize")));
    assert!(
        fixture.requests.try_recv().is_err(),
        "only one initialize may reach upstream"
    );
    Ok((BufReader::new(first), BufReader::new(second), leader))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_socket_initialize_has_one_leader_and_one_shared_lifecycle() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let (mut first, mut second, leader) = two_clients(&mut fixture).await?;
    let result = json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}}});
    fixture
        .responses
        .send(json!({"jsonrpc":"2.0","id":leader.get("id"),"result":result}).to_string())
        .await
        .map_err(io::Error::other)?;
    let (first_response, second_response) = tokio::join!(read(&mut first), read(&mut second));
    assert_eq!(
        first_response?,
        json!({"jsonrpc":"2.0","id":1,"result":result})
    );
    assert_eq!(
        second_response?,
        json!({"jsonrpc":"2.0","id":"second","result":result})
    );
    assert!(fixture.proxy.readiness().mcp_initialize_result_received);

    let state = fixture
        .proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    let capabilities = state
        .client_capabilities
        .lock()
        .values()
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        capabilities
            .iter()
            .any(|value| value.sampling && !value.roots)
    );
    assert!(
        capabilities
            .iter()
            .any(|value| value.roots && !value.sampling)
    );

    // Per-client ping barriers prove both initialized notifications were consumed.
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":101,"method":"ping"}),
    )
    .await?;
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await?;
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":202,"method":"ping"}),
    )
    .await?;
    let mut initialized = 0;
    let mut pings = 0;
    for _ in 0..3 {
        let request = fixture.request().await?;
        match request.get("method").and_then(Value::as_str) {
            Some("notifications/initialized") => initialized += 1,
            Some("ping") => pings += 1,
            other => return Err(io::Error::other(format!("unexpected method {other:?}"))),
        }
    }
    assert_eq!(initialized, 1);
    assert_eq!(pings, 2);
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}

async fn initialize_error() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let (mut first, mut second, leader) = two_clients(&mut fixture).await?;
    let error = json!({"code":-32602,"message":"initialization rejected"});
    let id = leader.get("id").cloned().unwrap_or(Value::Null);
    fixture
        .responses
        .send(json!({"jsonrpc":"2.0","id":id,"error":error}).to_string())
        .await
        .map_err(io::Error::other)?;
    let (first_response, second_response) = tokio::join!(read(&mut first), read(&mut second));
    assert_eq!(
        first_response?,
        json!({"jsonrpc":"2.0","id":1,"error":error})
    );
    assert_eq!(
        second_response?,
        json!({"jsonrpc":"2.0","id":"second","error":error})
    );
    assert!(!fixture.proxy.readiness().mcp_initialize_result_received);
    assert!(
        fixture.requests.try_recv().is_err(),
        "failed initialization is not automatically replayed"
    );

    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}),
    )
    .await?;
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":"retry","method":"initialize","params":{}}),
    )
    .await?;
    let retry = fixture.request().await?;
    follower_queued(&fixture.proxy).await?;
    assert!(fixture.requests.try_recv().is_err());
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":retry.get("id"),"result":{"capabilities":{}}}).to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut first).await?.get("id"), Some(&json!(2)));
    assert_eq!(read(&mut second).await?.get("id"), Some(&json!("retry")));
    fixture.stop().await
}

#[tokio::test]
async fn initialize_error_fans_out_and_explicit_retry_coalesces() -> io::Result<()> {
    initialize_error().await
}

#[tokio::test]
async fn stop_disconnects_initialize_waiters_before_confirming_retirement() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let (mut first, mut second, _) = two_clients(&mut fixture).await?;
    let generation = fixture
        .proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    let proxy = fixture.proxy.clone();
    let stop = tokio::spawn(async move { proxy.stop().await });
    fixture.shutdown.await.map_err(io::Error::other)?;
    let mut first_line = String::new();
    let mut second_line = String::new();
    let sizes = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            first.read_line(&mut first_line),
            second.read_line(&mut second_line)
        )
    })
    .await
    .map_err(io::Error::other)?;
    assert_eq!(sizes.0?, 0);
    assert_eq!(sizes.1?, 0);
    assert!(!stop.is_finished());
    fixture.retired.send_replace(Some(Ok(())));
    stop.await.map_err(io::Error::other)??;
    assert!(matches!(
        generation.handshake_cache.lock().initialize,
        Initialization::Empty
    ));
    assert!(generation.request_map.lock().is_empty());
    Ok(())
}

#[tokio::test]
async fn disconnected_initialize_leader_does_not_strand_follower() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = connect(&fixture.proxy).await?;
    send(
        &mut first,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await?;
    let leader = fixture.request().await?;
    let mut second = connect(&fixture.proxy).await?;
    send(
        &mut second,
        json!({"jsonrpc":"2.0","id":"second","method":"initialize","params":{}}),
    )
    .await?;
    follower_queued(&fixture.proxy).await?;
    drop(first);
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.proxy.connection_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":leader.get("id"),"result":{"capabilities":{}}}).to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    let mut second = BufReader::new(second);
    assert_eq!(read(&mut second).await?.get("id"), Some(&json!("second")));
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await?;
    assert_eq!(
        fixture.request().await?.get("method"),
        Some(&json!("notifications/initialized"))
    );
    fixture.stop().await
}

#[test]
fn stale_initialize_leader_fails_leader_and_followers_without_touching_tools_waiters() {
    let cache = Arc::new(Mutex::new(HandshakeCache::default()));
    assert!(matches!(
        prepare_initialize_request(
            &cache,
            "first",
            json!(1),
            Duration::from_secs(REQUEST_TTL_SECS),
            Instant::now()
        ),
        DiscoveryAction::Leader
    ));
    assert!(matches!(
        prepare_initialize_request(
            &cache,
            "second",
            json!(2),
            Duration::from_secs(REQUEST_TTL_SECS),
            Instant::now()
        ),
        DiscoveryAction::Coalesced
    ));
    assert!(matches!(
        prepare_tools_list_request(
            &cache,
            "tools-first",
            json!(3),
            Duration::from_secs(REQUEST_TTL_SECS),
            Instant::now()
        ),
        DiscoveryAction::Leader
    ));
    assert!(matches!(
        prepare_tools_list_request(
            &cache,
            "tools-second",
            json!(4),
            Duration::from_secs(REQUEST_TTL_SECS),
            Instant::now()
        ),
        DiscoveryAction::Coalesced
    ));
    let stale = PendingRequestInfo {
        client_id: "first".to_string(),
        original_id: json!(1),
        method: Some("initialize".to_string()),
        cache_key: Some(CacheableMethod::Initialize),
        tool: None,
        inserted_at: Instant::now() - Duration::from_secs(REQUEST_TTL_SECS + 1),
        expires_after: Duration::from_secs(REQUEST_TTL_SECS),
    };
    let responses = cleanup_initialize_after_stale_requests(&cache, &[stale]);
    assert_eq!(responses.len(), 2);
    assert!(
        responses
            .iter()
            .all(|(_, payload)| payload.contains("initialize timed out"))
    );
    assert!(matches!(cache.lock().initialize, Initialization::Empty));
    assert!(cache.lock().tools_list.in_flight.is_some());
    assert_eq!(cache.lock().tools_list.waiters.len(), 1);
}

#[path = "socket_proxy_expiration_tests.rs"]
mod expiration_tests;

#[path = "socket_proxy_deadline_tests.rs"]
mod deadline_tests;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_not_found_initialize_error_reaches_both_clients_before_recovery() -> io::Result<()>
{
    let mut fixture = Fixture::start().await?;
    let (mut first, mut second, leader) = two_clients(&mut fixture).await?;
    let error = json!({"code":-32001,"message":"Session not found"});
    fixture
        .responses
        .send(json!({"jsonrpc":"2.0","id":leader.get("id"),"error":error}).to_string())
        .await
        .map_err(io::Error::other)?;
    let (first_response, second_response) = tokio::join!(read(&mut first), read(&mut second));
    assert_eq!(
        first_response?,
        json!({"jsonrpc":"2.0","id":1,"error":error})
    );
    assert_eq!(
        second_response?,
        json!({"jsonrpc":"2.0","id":"second","error":error})
    );
    assert!(fixture.requests.try_recv().is_err());
    fixture.proxy.request_stop();
    fixture.shutdown.await.map_err(io::Error::other)?;
    fixture.retired.send_replace(Some(Ok(())));
    fixture.proxy.stop().await?;
    Ok(())
}

#[tokio::test]
async fn slow_client_does_not_block_verified_retirement() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut client = connect(&fixture.proxy).await?;
    send(
        &mut client,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
    )
    .await?;
    let leader = fixture.request().await?;
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":leader.get("id"),"result":{
                "capabilities":{}, "padding":"x".repeat(2 * 1024 * 1024)
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    let mut prefix = [0u8; 1];
    tokio::time::timeout(Duration::from_secs(2), client.read_exact(&mut prefix))
        .await
        .map_err(io::Error::other)??;
    assert_eq!(prefix, *b"{");
    tokio::time::timeout(Duration::from_secs(2), fixture.stop())
        .await
        .map_err(io::Error::other)??;
    drop(client);
    Ok(())
}
