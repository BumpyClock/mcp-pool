use super::*;
use crate::mcp_client::McpClient;

#[derive(Clone, Copy)]
enum ResponseKind {
    Headers,
    JsonBody,
    SseBody,
}

struct HttpFixture {
    proxy: Arc<SocketProxy>,
    requests: mpsc::Receiver<Value>,
    release: Arc<Semaphore>,
    count: Arc<AtomicU32>,
    shutdown: oneshot::Sender<()>,
    server: tokio::task::JoinHandle<io::Result<()>>,
}

async fn connection(
    stream: TcpStream,
    target: &'static str,
    kind: ResponseKind,
    requests: mpsc::Sender<Value>,
    release: Arc<Semaphore>,
    count: Arc<AtomicU32>,
) -> io::Result<()> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await?;
    let verb = line.split_whitespace().next().unwrap_or("").to_string();
    let mut length = 0;
    loop {
        line.clear();
        if reader.read_line(&mut line).await? == 0 || line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse::<usize>().map_err(io::Error::other)?;
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    if verb != "POST" {
        reader
            .get_mut()
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await?;
        return Ok(());
    }
    let request: Value = serde_json::from_slice(&body)?;
    assert!(
        request
            .get(crate::request_deadline::TIMEOUT_FIELD)
            .is_none()
    );
    let method = request.get("method").and_then(Value::as_str);
    let held = method == Some(target);
    if held {
        count.fetch_add(1, Ordering::SeqCst);
        requests
            .send(request.clone())
            .await
            .map_err(io::Error::other)?;
    }
    let result = match method {
        Some("initialize") => json!({
            "protocolVersion":"2025-03-26","capabilities":{"tools":{}},
            "serverInfo":{"name":"shared-deadline-fixture","version":"1"}
        }),
        Some("tools/list") => json!({"tools":[]}),
        _ => json!({}),
    };
    let body = if request.get("id").is_some() {
        json!({"jsonrpc":"2.0","id":request.get("id"),"result":result}).to_string()
    } else {
        String::new()
    };
    let sse = held && matches!(kind, ResponseKind::SseBody);
    let body = if sse {
        format!("event: message\ndata: {body}\n\n")
    } else {
        body
    };
    let content_type = if sse {
        "text/event-stream"
    } else {
        "application/json"
    };
    let status = if request.get("id").is_some() {
        "200 OK"
    } else {
        "202 Accepted"
    };
    if held && matches!(kind, ResponseKind::Headers) {
        release.acquire().await.map_err(io::Error::other)?.forget();
    }
    reader.get_mut().write_all(format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()
    ).as_bytes()).await?;
    if held && !matches!(kind, ResponseKind::Headers) {
        release.acquire().await.map_err(io::Error::other)?.forget();
    }
    reader.get_mut().write_all(body.as_bytes()).await?;
    reader.get_mut().shutdown().await
}

impl HttpFixture {
    async fn start(target: &'static str, kind: ResponseKind, floor_ms: u64) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let (requests_tx, requests) = mpsc::channel(8);
        let release = Arc::new(Semaphore::new(0));
        let count = Arc::new(AtomicU32::new(0));
        let server_release = release.clone();
        let server_count = count.clone();
        let (shutdown, mut stopped) = oneshot::channel();
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let (stream, _) = accepted?;
                        connections.spawn(connection(stream, target, kind, requests_tx.clone(),
                            server_release.clone(), server_count.clone()));
                    }
                    result = connections.join_next(), if !connections.is_empty() => {
                        if let Some(result) = result {
                            result.map_err(io::Error::other)??;
                        }
                    }
                }
            }
            connections.abort_all();
            while let Some(result) = connections.join_next().await {
                match result {
                    Ok(result) => result?,
                    Err(error) if error.is_cancelled() => {}
                    Err(error) => return Err(io::Error::other(error)),
                }
            }
            Ok(())
        });
        let identity = super::super::lifecycle_tests::proxy();
        let proxy = Arc::new(SocketProxy::new(
            format!("shared-budget-{target}"),
            identity.socket_path(),
            UpstreamSpec::Http {
                url: format!("http://{address}/mcp"),
                sse: false,
                headers: Default::default(),
                timeout_ms: Some(floor_ms),
                auth: None,
            },
            true,
            None,
        ));
        proxy.start().await?;
        Ok(Self {
            proxy,
            requests,
            release,
            count,
            shutdown,
            server,
        })
    }

    async fn accepted(&mut self) -> io::Result<Value> {
        tokio::time::timeout(Duration::from_secs(2), self.requests.recv())
            .await
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("HTTP request fixture stopped"))
    }

    async fn client(&self, milliseconds: u64) -> anyhow::Result<McpClient> {
        McpClient::initialize(
            connect(&self.proxy).await?,
            Duration::from_millis(milliseconds),
        )
        .await
    }

    async fn stop(self) -> io::Result<()> {
        self.proxy.stop().await?;
        self.shutdown
            .send(())
            .map_err(|_| io::Error::other("HTTP fixture already stopped"))?;
        self.server.await.map_err(io::Error::other)??;
        Ok(())
    }
}

async fn wait_for_shared_follower(proxy: &SocketProxy, target: &str) -> io::Result<()> {
    let generation = proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let queued = {
                let cache = generation.handshake_cache.lock();
                if target == "initialize" {
                    matches!(&cache.initialize, Initialization::InFlight { waiters, .. } if !waiters.is_empty())
                } else {
                    !cache.tools_list.waiters.is_empty()
                }
            };
            if queued { break; }
            tokio::task::yield_now().await;
        }
    }).await.map_err(io::Error::other)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_initialize_extends_header_and_body_waits_without_replay() -> anyhow::Result<()> {
    for kind in [
        ResponseKind::Headers,
        ResponseKind::JsonBody,
        ResponseKind::SseBody,
    ] {
        let mut fixture = HttpFixture::start("initialize", kind, 100).await?;
        let short_proxy = fixture.proxy.clone();
        let short = tokio::spawn(async move {
            McpClient::initialize(connect(&short_proxy).await?, Duration::from_millis(300)).await
        });
        fixture.accepted().await?;
        let long_proxy = fixture.proxy.clone();
        let long = tokio::spawn(async move {
            McpClient::initialize(connect(&long_proxy).await?, Duration::from_millis(1800)).await
        });
        wait_for_shared_follower(&fixture.proxy, "initialize").await?;
        let error = short
            .await?
            .err()
            .ok_or_else(|| io::Error::other("short caller did not time out"))?;
        assert!(
            error
                .to_string()
                .contains("MCP initialize deadline exceeded")
        );
        tokio::time::sleep(Duration::from_millis(300)).await;
        fixture.release.add_permits(1);
        let mut client = long.await??;
        assert!(!client.is_closed());
        assert_eq!(fixture.count.load(Ordering::SeqCst), 1);
        assert!(fixture.requests.try_recv().is_err());
        let cached = fixture.client(300).await?;
        assert!(!cached.is_closed());
        assert_eq!(fixture.count.load(Ordering::SeqCst), 1);
        client
            .notify("notifications/initialized", json!({}))
            .await?;
        fixture.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shared_discovery_extends_existing_wait_and_keeps_each_local_deadline() -> anyhow::Result<()>
{
    let mut fixture = HttpFixture::start("tools/list", ResponseKind::JsonBody, 100).await?;
    let mut short_client = fixture.client(300).await?;
    let mut long_client = fixture.client(1800).await?;
    let short = tokio::spawn(async move {
        let result = short_client.request("tools/list", json!({})).await;
        (result, short_client.is_closed())
    });
    fixture.accepted().await?;
    let long = tokio::spawn(async move { long_client.list_tools().await });
    wait_for_shared_follower(&fixture.proxy, "tools/list").await?;
    let (result, closed) = short.await?;
    assert!(
        result
            .err()
            .ok_or_else(|| io::Error::other("short discovery succeeded"))?
            .to_string()
            .contains("MCP tools/list deadline exceeded")
    );
    assert!(closed);
    tokio::time::sleep(Duration::from_millis(300)).await;
    fixture.release.add_permits(1);
    assert!(
        long.await?
            .map_err(|error| io::Error::other(format!("shared discovery: {error:#}")))?
            .is_empty()
    );
    assert_eq!(fixture.count.load(Ordering::SeqCst), 1);
    let mut cached = fixture
        .client(300)
        .await
        .map_err(|error| io::Error::other(format!("cached initialization: {error:#}")))?;
    assert!(
        cached
            .list_tools()
            .await
            .map_err(|error| io::Error::other(format!("cached discovery: {error:#}")))?
            .is_empty()
    );
    assert_eq!(fixture.count.load(Ordering::SeqCst), 1);
    fixture.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_initialize_accepts_a_long_follower_after_leader_disconnect() -> anyhow::Result<()>
{
    let mut fixture = HttpFixture::start("initialize", ResponseKind::JsonBody, 1200).await?;
    let short_proxy = fixture.proxy.clone();
    let short = tokio::spawn(async move {
        McpClient::initialize(connect(&short_proxy).await?, Duration::from_millis(250)).await
    });
    fixture.accepted().await?;
    assert!(short.await?.is_err());
    let long_proxy = fixture.proxy.clone();
    let long = tokio::spawn(async move {
        McpClient::initialize(connect(&long_proxy).await?, Duration::from_millis(2400)).await
    });
    wait_for_shared_follower(&fixture.proxy, "initialize").await?;
    tokio::time::sleep(Duration::from_millis(1300)).await;
    fixture.release.add_permits(1);
    assert!(!long.await??.is_closed());
    assert_eq!(fixture.count.load(Ordering::SeqCst), 1);
    fixture.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unanswered_shared_operations_expire_and_release_routes_without_replay()
-> anyhow::Result<()> {
    for target in ["initialize", "tools/list"] {
        let mut fixture = HttpFixture::start(target, ResponseKind::JsonBody, 100).await?;
        let mut first = BufReader::new(connect(&fixture.proxy).await?);
        let mut follower = BufReader::new(connect(&fixture.proxy).await?);
        send(
            first.get_mut(),
            json!({
                "jsonrpc":"2.0","id":"leader","method":target,"_mcp_pool_timeout_ms":250
            }),
        )
        .await?;
        fixture.accepted().await?;
        send(
            follower.get_mut(),
            json!({
                "jsonrpc":"2.0","id":"follower","method":target,"_mcp_pool_timeout_ms":800
            }),
        )
        .await?;
        wait_for_shared_follower(&fixture.proxy, target).await?;
        assert!(
            tokio::time::timeout(Duration::from_millis(400), read(&mut follower))
                .await
                .is_err()
        );
        let error = read(&mut follower).await?;
        assert_eq!(error.get("id"), Some(&json!("follower")));
        assert!(
            error
                .pointer("/error/message")
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains("deadline exceeded"))
        );
        assert!(error.get(crate::request_deadline::TIMEOUT_FIELD).is_none());
        assert!(read(&mut first).await?.get("error").is_some());
        let generation = fixture
            .proxy
            .generation
            .lock()
            .clone()
            .ok_or_else(|| io::Error::other("missing generation"))?;
        assert!(generation.request_map.lock().is_empty());
        {
            let cache = generation.handshake_cache.lock();
            assert!(matches!(cache.initialize, Initialization::Empty));
            assert!(cache.tools_list.in_flight.is_none());
            assert!(cache.tools_list.waiters.is_empty());
        }
        assert_eq!(fixture.count.load(Ordering::SeqCst), 1);
        assert!(fixture.requests.try_recv().is_err());
        fixture.stop().await?;
    }
    Ok(())
}
