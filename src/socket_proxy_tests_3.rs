use super::*;
use serde_json::json;

// Fresh, empty per-upstream handshake cache for route_response tests.
fn empty_cache() -> HandshakeCacheRef {
    Arc::new(Mutex::new(HandshakeCache::default()))
}

fn pending_request(
    client_id: &str,
    original_id: Value,
    method: Option<&str>,
) -> PendingRequestInfo {
    PendingRequestInfo {
        client_id: client_id.to_string(),
        original_id,
        method: method.map(str::to_string),
        cache_key: method.and_then(cacheable_method),
        tool: None,
        inserted_at: Instant::now(),
    }
}

fn stale_pending_request(
    client_id: &str,
    original_id: Value,
    method: Option<&str>,
) -> PendingRequestInfo {
    PendingRequestInfo {
        client_id: client_id.to_string(),
        original_id,
        method: method.map(str::to_string),
        cache_key: method.and_then(cacheable_method),
        tool: None,
        inserted_at: Instant::now() - Duration::from_secs(REQUEST_TTL_SECS + 1),
    }
}

#[test]
fn successful_tools_list_leader_fans_out_and_populates_cache() {
    let cache = empty_cache();
    cache.lock().tools_list.in_flight = true;
    cache.lock().tools_list.waiters.push(PendingWaiter {
        client_id: "clientB".to_string(),
        original_id: json!(2),
        inserted_at: Instant::now(),
    });
    let leader = pending_request("clientA", json!(1), Some("tools/list"));

    let responses = complete_tools_list_response(
        &json!({"jsonrpc":"2.0","id":10,"result":{"tools":["t1"]}}),
        leader,
        &cache,
    );

    assert_eq!(responses.len(), 2, "leader and waiter both answered");
    let leader_response: Value = serde_json::from_str(&responses[0].1).expect("valid json");
    let waiter_response: Value = serde_json::from_str(&responses[1].1).expect("valid json");
    assert_eq!(leader_response["id"], json!(1));
    assert_eq!(waiter_response["id"], json!(2));
    let guard = cache.lock();
    assert!(!guard.tools_list.in_flight);
    assert!(guard.tools_list.waiters.is_empty());
    assert_eq!(
        guard.tools_list.cached_result,
        Some(json!({"tools":["t1"]}))
    );
}

#[test]
fn stale_tools_list_leader_clears_in_flight_and_drains_waiters() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    request_map.lock().insert(
        "10".to_string(),
        stale_pending_request("leader", json!(1), Some("tools/list")),
    );
    let cleanup_counter = Arc::new(AtomicU32::new(0));
    let cache = empty_cache();
    cache.lock().tools_list.in_flight = true;
    cache.lock().tools_list.waiters.push(PendingWaiter {
        client_id: "waiter".to_string(),
        original_id: json!(2),
        inserted_at: Instant::now(),
    });

    let stale_requests = cleanup_stale_requests(&request_map, &cleanup_counter);
    let responses = cleanup_tools_list_after_stale_requests(&cache, &stale_requests);

    assert!(request_map.lock().is_empty(), "stale leader removed");
    assert_eq!(responses.len(), 1, "waiter receives timeout error");
    let timeout: Value = serde_json::from_str(&responses[0].1).expect("valid json");
    assert_eq!(responses[0].0, "waiter");
    assert_eq!(timeout["id"], json!(2));
    assert_eq!(timeout["error"]["code"], json!(-32001));
    assert_eq!(
        timeout["error"]["message"],
        json!("tools/list discovery timed out")
    );
    assert!(!cache.lock().tools_list.in_flight, "in-flight cleared");
    assert!(
        cache.lock().tools_list.waiters.is_empty(),
        "waiters drained"
    );

    let next = prepare_tools_list_request(&cache, "next", json!(3));

    assert!(
        matches!(next, DiscoveryAction::Leader),
        "future miss can elect a new leader"
    );
    assert!(
        cache.lock().tools_list.in_flight,
        "new leader owns discovery"
    );
}

#[test]
fn failed_tools_list_leader_fans_out_without_caching_error() {
    let cache = empty_cache();
    cache.lock().store("tools/list", json!({"tools":["old"]}));
    cache.lock().invalidate_tools_list();
    cache.lock().tools_list.in_flight = true;
    cache.lock().tools_list.waiters.push(PendingWaiter {
        client_id: "clientB".to_string(),
        original_id: json!(2),
        inserted_at: Instant::now(),
    });
    let leader = pending_request("clientA", json!(1), Some("tools/list"));

    let responses = complete_tools_list_response(
        &json!({"jsonrpc":"2.0","id":10,"error":{"code":429,"message":"too many"}}),
        leader,
        &cache,
    );

    assert_eq!(responses.len(), 2);
    let leader_response: Value = serde_json::from_str(&responses[0].1).expect("valid json");
    let waiter_response: Value = serde_json::from_str(&responses[1].1).expect("valid json");
    assert_eq!(leader_response["id"], json!(1));
    assert_eq!(waiter_response["id"], json!(2));
    assert_eq!(leader_response["error"]["code"], json!(429));
    let guard = cache.lock();
    assert_eq!(guard.tools_list.cached_result, None, "error not cached");
}

#[test]
fn later_success_repopulates_tools_list_cache_after_failure() {
    let cache = empty_cache();
    cache.lock().tools_list.in_flight = true;
    let failed_leader = pending_request("clientA", json!(1), Some("tools/list"));
    let _responses = complete_tools_list_response(
        &json!({"jsonrpc":"2.0","id":10,"error":{"code":429,"message":"too many"}}),
        failed_leader,
        &cache,
    );
    cache.lock().tools_list.in_flight = true;

    let successful_leader = pending_request("clientA", json!(3), Some("tools/list"));
    let _responses = complete_tools_list_response(
        &json!({"jsonrpc":"2.0","id":11,"result":{"tools":["new"]}}),
        successful_leader,
        &cache,
    );

    assert_eq!(
        cache.lock().tools_list.cached_result,
        Some(json!({"tools":["new"]}))
    );
}
