use super::*;

async fn age_registered_requests(proxy: &SocketProxy, count: usize) -> io::Result<()> {
    let generation = proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while generation.request_map.lock().len() != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    for request in generation.request_map.lock().values_mut() {
        request.inserted_at =
            Instant::now() - Duration::from_secs(REQUEST_TTL_SECS) + Duration::from_millis(50);
    }
    generation.expiration_changed.notify_one();
    Ok(())
}

#[tokio::test]
async fn unpublished_backend_expires_socket_leader_and_followers_before_dispatch() -> io::Result<()>
{
    let proxy = proxy();
    let Backend {
        setup,
        handle,
        responses,
        requests,
        shutdown,
        retired,
    } = backend(&proxy);
    let start = {
        let proxy = proxy.clone();
        tokio::spawn(async move { proxy.start().await })
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        while !proxy.readiness().local_socket_bound {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    let mut first = BufReader::new(connect(&proxy).await?);
    let mut second = BufReader::new(connect(&proxy).await?);
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"old","method":"initialize","params":{}}),
    )
    .await?;
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":"follower","method":"initialize","params":{}}),
    )
    .await?;
    follower_queued(&proxy).await?;
    age_registered_requests(&proxy, 1).await?;
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":"old","error":{"code":-32001,"message":"initialize timed out"}})
    );
    assert_eq!(
        read(&mut second).await?,
        json!({"jsonrpc":"2.0","id":"follower","error":{"code":-32001,"message":"initialize timed out"}})
    );
    assert!(!start.is_finished(), "timeouts precede sender publication");
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"retry","method":"initialize","params":{}}),
    )
    .await?;
    let generation = proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while generation.request_map.lock().len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    setup
        .send(Ok(handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    start.await.map_err(io::Error::other)??;
    let mut fixture = Fixture {
        proxy,
        responses,
        requests,
        shutdown,
        retired,
    };
    let retry = fixture.request().await?;
    assert_eq!(retry.get("method"), Some(&json!("initialize")));
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":retry.get("id"),"result":{"capabilities":{}}}).to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut first).await?.get("id"), Some(&json!("retry")));
    assert!(
        fixture.requests.try_recv().is_err(),
        "expired leader is never dispatched"
    );
    fixture.stop().await
}

#[tokio::test]
async fn upstream_backpressure_expires_socket_request_and_preserves_input_order() -> io::Result<()>
{
    let mut fixture = Fixture::start().await?;
    let generation = fixture
        .proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    let sender = generation
        .request_tx
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing sender"))?;
    for _ in 0..16 {
        sender
            .send("occupied".into())
            .await
            .map_err(io::Error::other)?;
    }
    let mut client = BufReader::new(connect(&fixture.proxy).await?);
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"old","method":"ping"}),
    )
    .await?;
    age_registered_requests(&fixture.proxy, 1).await?;
    assert_eq!(
        read(&mut client).await?,
        json!({"jsonrpc":"2.0","id":"old","error":{"code":-32001,"message":"request timed out"}})
    );
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","method":"notifications/first"}),
    )
    .await?;
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"fresh","method":"ping"}),
    )
    .await?;
    for _ in 0..16 {
        assert_eq!(fixture.requests.recv().await.as_deref(), Some("occupied"));
    }
    assert_eq!(
        fixture.request().await?.get("method"),
        Some(&json!("notifications/first"))
    );
    let fresh = fixture.request().await?;
    assert_eq!(fresh.get("method"), Some(&json!("ping")));
    fixture
        .responses
        .send(json!({"jsonrpc":"2.0","id":fresh.get("id"),"result":{}}).to_string())
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut client).await?.get("id"), Some(&json!("fresh")));
    assert!(
        fixture.requests.try_recv().is_err(),
        "expired request is never dispatched"
    );
    fixture.stop().await
}

#[tokio::test]
async fn stop_retires_client_with_backpressured_forward() -> io::Result<()> {
    let fixture = Fixture::start().await?;
    let generation = fixture
        .proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    let sender = generation
        .request_tx
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing sender"))?;
    for _ in 0..16 {
        sender
            .send("occupied".into())
            .await
            .map_err(io::Error::other)?;
    }
    let mut client = connect(&fixture.proxy).await?;
    send(
        &mut client,
        json!({"jsonrpc":"2.0","id":"blocked","method":"ping"}),
    )
    .await?;
    age_registered_requests(&fixture.proxy, 1).await?;
    tokio::time::timeout(Duration::from_secs(2), fixture.stop())
        .await
        .map_err(io::Error::other)??;
    assert!(generation.clients.lock().is_empty());
    assert!(generation.request_map.lock().is_empty());
    Ok(())
}

#[tokio::test]
async fn disconnected_initialize_leader_still_expires_followers() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let (first, mut second, _) = two_clients(&mut fixture).await?;
    drop(first);
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.proxy.connection_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    let generation = fixture
        .proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    {
        let mut pending = generation.request_map.lock();
        assert_eq!(
            pending.len(),
            1,
            "disconnected leader retains the shared deadline"
        );
        for request in pending.values_mut() {
            request.inserted_at = Instant::now() - Duration::from_secs(REQUEST_TTL_SECS + 1);
        }
    }
    generation.expiration_changed.notify_one();
    assert_eq!(
        read(&mut second).await?,
        json!({"jsonrpc":"2.0","id":"second","error":{"code":-32001,"message":"initialize timed out"}})
    );
    assert!(generation.request_map.lock().is_empty());
    assert!(matches!(
        generation.handshake_cache.lock().initialize,
        Initialization::Empty
    ));
    assert!(
        fixture.requests.try_recv().is_err(),
        "expiration must not replay"
    );
    fixture.stop().await
}

#[tokio::test]
async fn silent_upstream_deadline_expires_clients_and_allows_explicit_retry() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let (mut first, mut second, _) = two_clients(&mut fixture).await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"tools-a","method":"tools/list"}),
    )
    .await?;
    fixture.request().await?;
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":"tools-b","method":"tools/list"}),
    )
    .await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"normal","method":"ping"}),
    )
    .await?;
    fixture.request().await?;
    let generation = fixture
        .proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while generation.handshake_cache.lock().tools_list.waiters.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    let near_deadline =
        Instant::now() - Duration::from_secs(REQUEST_TTL_SECS) + Duration::from_millis(100);
    {
        let mut pending = generation.request_map.lock();
        for request in pending.values_mut() {
            if request.cache_key != Some(CacheableMethod::ToolsList) {
                request.inserted_at = near_deadline;
            }
        }
        let mut cache = generation.handshake_cache.lock();
        for waiter in &mut cache.tools_list.waiters {
            waiter.inserted_at = near_deadline;
        }
    }
    generation.expiration_changed.notify_one();
    let first_one = read(&mut first).await?;
    let first_two = read(&mut first).await?;
    let second_one = read(&mut second).await?;
    let second_two = read(&mut second).await?;
    for response in [&first_one, &first_two, &second_one, &second_two] {
        assert_eq!(
            response.get("error").and_then(|error| error.get("code")),
            Some(&json!(-32001))
        );
    }
    let first_ids = [first_one.get("id"), first_two.get("id")];
    assert!(first_ids.contains(&Some(&json!(1))));
    assert!(first_ids.contains(&Some(&json!("normal"))));
    let second_ids = [second_one.get("id"), second_two.get("id")];
    assert!(second_ids.contains(&Some(&json!("second"))));
    assert!(second_ids.contains(&Some(&json!("tools-b"))));
    assert_eq!(
        generation.request_map.lock().len(),
        1,
        "tools leader remains live"
    );
    assert!(generation.handshake_cache.lock().tools_list.in_flight);
    assert!(fixture.requests.try_recv().is_err(), "no automatic replay");
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":"tools-new","method":"tools/list"}),
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while generation.handshake_cache.lock().tools_list.waiters.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    for request in generation.request_map.lock().values_mut() {
        request.inserted_at =
            Instant::now() - Duration::from_secs(REQUEST_TTL_SECS) + Duration::from_millis(50);
    }
    generation.expiration_changed.notify_one();
    let tools_timeout = read(&mut first).await?;
    assert_eq!(tools_timeout.get("id"), Some(&json!("tools-a")));
    assert!(tools_timeout.get("error").is_some());
    assert_eq!(
        read(&mut second).await?.get("id"),
        Some(&json!("tools-new"))
    );
    assert!(!generation.handshake_cache.lock().tools_list.in_flight);
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"tools-retry","method":"tools/list"}),
    )
    .await?;
    let tools_retry = fixture.request().await?;
    fixture
        .responses
        .send(json!({"jsonrpc":"2.0","id":tools_retry.get("id"),"result":{"tools":[]}}).to_string())
        .await
        .map_err(io::Error::other)?;
    assert_eq!(
        read(&mut first).await?.get("id"),
        Some(&json!("tools-retry"))
    );
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"retry","method":"initialize","params":{}}),
    )
    .await?;
    let retry = fixture.request().await?;
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":retry.get("id"),"result":{"capabilities":{}}}).to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut first).await?.get("id"), Some(&json!("retry")));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), read(&mut second))
            .await
            .is_err(),
        "expired followers receive no duplicate response"
    );
    fixture.stop().await
}
