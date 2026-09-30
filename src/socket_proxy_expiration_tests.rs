use super::*;

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
