use super::*;

#[tokio::test]
async fn shared_http_routes_keep_the_configured_deadline_floor() -> io::Result<()> {
    let mut proxy = proxy();
    Arc::get_mut(&mut proxy)
        .ok_or_else(|| io::Error::other("unexpected shared test proxy"))?
        .spec = UpstreamSpec::Http {
        url: "https://example.invalid/mcp".into(),
        sse: false,
        headers: Default::default(),
        timeout_ms: Some(700_000),
        auth: None,
    };
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
    let mut fixture = Fixture {
        proxy,
        responses,
        requests,
        shutdown,
        retired,
    };
    let mut client = BufReader::new(connect(&fixture.proxy).await?);
    send(
        client.get_mut(),
        json!({
            "jsonrpc":"2.0","id":"shared","method":"initialize","params":{},
            "_mcp_pool_timeout_ms":20_000
        }),
    )
    .await?;
    let forwarded = fixture.request().await?;
    assert_eq!(
        forwarded.get(crate::request_deadline::TIMEOUT_FIELD),
        Some(&json!(20_000))
    );
    let generation = fixture
        .proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    for request in generation.request_map.lock().values_mut() {
        assert_eq!(request.expires_after, Duration::from_secs(700));
        request.inserted_at = Instant::now() - Duration::from_secs(301);
    }
    assert!(
        expire_pending_requests(&generation.request_map, &generation.handshake_cache).is_empty()
    );
    fixture.stop().await
}

#[tokio::test]
async fn long_requests_remain_routable_beyond_the_default_ttl() -> io::Result<()> {
    for method in ["initialize", "tools/list", "tools/call"] {
        let mut fixture = Fixture::start().await?;
        let mut client = BufReader::new(connect(&fixture.proxy).await?);
        send(
            client.get_mut(),
            json!({
                "jsonrpc":"2.0", "id":"long", "method":method, "params":{},
                "_mcp_pool_timeout_ms":600_000
            }),
        )
        .await?;
        let forwarded = fixture.request().await?;
        assert!(
            forwarded
                .get(crate::request_deadline::TIMEOUT_FIELD)
                .is_none(),
            "stdio upstreams must not receive local routing metadata"
        );
        let generation = fixture
            .proxy
            .generation
            .lock()
            .clone()
            .ok_or_else(|| io::Error::other("missing generation"))?;
        for request in generation.request_map.lock().values_mut() {
            assert_eq!(request.expires_after, Duration::from_secs(600));
            request.inserted_at = Instant::now() - Duration::from_secs(301);
        }
        assert!(
            expire_pending_requests(&generation.request_map, &generation.handshake_cache)
                .is_empty()
        );
        fixture
            .responses
            .send(
                json!({"jsonrpc":"2.0","id":forwarded.get("id"),"result":{"accepted":true}})
                    .to_string(),
            )
            .await
            .map_err(io::Error::other)?;
        assert_eq!(
            read(&mut client).await?,
            json!({"jsonrpc":"2.0","id":"long","result":{"accepted":true}})
        );
        fixture.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn long_discovery_followers_keep_their_routing_budget() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = BufReader::new(connect(&fixture.proxy).await?);
    let mut second = BufReader::new(connect(&fixture.proxy).await?);
    for (client, identifier) in [(&mut first, "first"), (&mut second, "second")] {
        send(
            client.get_mut(),
            json!({
                "jsonrpc":"2.0","id":identifier,"method":"tools/list",
                "_mcp_pool_timeout_ms":600_000
            }),
        )
        .await?;
    }
    let forwarded = fixture.request().await?;
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
    for pending in generation.request_map.lock().values_mut() {
        pending.inserted_at = Instant::now() - Duration::from_secs(301);
    }
    for waiter in &mut generation.handshake_cache.lock().tools_list.waiters {
        assert_eq!(waiter.expires_after, Duration::from_secs(600));
        waiter.inserted_at = Instant::now() - Duration::from_secs(301);
    }
    assert!(
        expire_pending_requests(&generation.request_map, &generation.handshake_cache).is_empty()
    );
    fixture
        .responses
        .send(json!({"jsonrpc":"2.0","id":forwarded.get("id"),"result":{"tools":[]}}).to_string())
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut first).await?.get("id"), Some(&json!("first")));
    assert_eq!(read(&mut second).await?.get("id"), Some(&json!("second")));
    assert!(
        fixture.requests.try_recv().is_err(),
        "discovery stays coalesced"
    );
    fixture.stop().await
}

#[tokio::test]
async fn long_requests_expire_at_their_own_budget_without_replay() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut client = BufReader::new(connect(&fixture.proxy).await?);
    send(
        client.get_mut(),
        json!({
            "jsonrpc":"2.0","id":"expired","method":"tools/call","params":{},
            "_mcp_pool_timeout_ms":600_000
        }),
    )
    .await?;
    fixture.request().await?;
    let generation = fixture
        .proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    for request in generation.request_map.lock().values_mut() {
        request.inserted_at = Instant::now() - Duration::from_secs(601);
    }
    generation.expiration_changed.notify_one();
    let response = read(&mut client).await?;
    assert_eq!(response.get("id"), Some(&json!("expired")));
    assert_eq!(
        response.get("error").and_then(|error| error.get("code")),
        Some(&json!(-32001))
    );
    assert!(generation.request_map.lock().is_empty());
    assert!(
        fixture.requests.try_recv().is_err(),
        "expiration must not replay"
    );
    fixture.stop().await
}

#[tokio::test]
async fn invalid_deadline_metadata_is_rejected_before_dispatch() -> io::Result<()> {
    let fixture = Fixture::start().await?;
    let mut client = BufReader::new(connect(&fixture.proxy).await?);
    send(
        client.get_mut(),
        json!({
            "jsonrpc":"2.0","id":"invalid","method":"tools/call","params":{},
            "_mcp_pool_timeout_ms":0
        }),
    )
    .await?;
    let response = read(&mut client).await?;
    assert_eq!(response.get("id"), Some(&json!("invalid")));
    assert_eq!(
        response.get("error").and_then(|error| error.get("code")),
        Some(&json!(-32602))
    );
    let mut fixture = fixture;
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}
