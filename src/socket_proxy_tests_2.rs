use super::*;
use serde_json::json;

fn channel_client(
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    id: &str,
) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel::<String>(8);
    clients.lock().insert(id.to_string(), tx);
    rx
}

fn last_active(client_id: Option<&str>) -> Arc<Mutex<Option<String>>> {
    Arc::new(Mutex::new(client_id.map(str::to_string)))
}

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
        expires_after: Duration::from_secs(REQUEST_TTL_SECS),
    }
}

async fn route_response(
    line: &str,
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    request_map: &RequestMap,
    handshake_cache: &HandshakeCacheRef,
    last_active_client: &Arc<Mutex<Option<String>>>,
) -> mpsc::Receiver<RecoveryReason> {
    let (recovery_tx, recovery_rx) = mpsc::channel::<RecoveryReason>(8);
    let client_capabilities = Arc::new(Mutex::new(HashMap::new()));
    if let Some(client_id) = last_active_client.lock().clone() {
        client_capabilities.lock().insert(
            client_id,
            ClientCapabilities {
                sampling: true,
                roots: true,
            },
        );
    }
    let request_tx = Arc::new(Mutex::new(None));
    super::route_response(
        line,
        clients,
        request_map,
        handshake_cache,
        last_active_client,
        &client_capabilities,
        &request_tx,
        &recovery_tx,
    )
    .await;
    recovery_rx
}

#[tokio::test]
async fn uncorrelated_session_error_preserves_caches_and_generation() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    request_map.lock().insert(
        "10".into(),
        pending_request("clientA", json!(99), Some("tools/call")),
    );
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let _rx_a = channel_client(&clients, "clientA");
    let last_active = last_active(None);
    let cache = empty_cache();
    cache
        .lock()
        .store("initialize", json!({"capabilities": {}}));
    cache.lock().store("tools/list", json!({"tools":["t1"]}));

    let mut recovery_rx = route_response(
        r#"{"jsonrpc":"2.0","id":"","error":{"code":-32001,"message":"Session not found"}}"#,
        &clients,
        &request_map,
        &cache,
        &last_active,
    )
    .await;

    assert!(cache.lock().get("initialize").is_some());
    assert!(cache.lock().tools_list.cached_result.is_some());
    assert!(recovery_rx.try_recv().is_err());
    assert_eq!(request_map.lock().len(), 1);
}

#[tokio::test]
async fn non_session_error_does_not_signal_recovery() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    request_map.lock().insert(
        "10".into(),
        pending_request("clientA", json!(99), Some("tools/call")),
    );
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let _rx_a = channel_client(&clients, "clientA");
    let last_active = last_active(None);
    let cache = empty_cache();
    cache
        .lock()
        .store("initialize", json!({"capabilities": {}}));

    let mut recovery_rx = route_response(
        r#"{"jsonrpc":"2.0","id":10,"error":{"code":-32001,"message":"rate limited"}}"#,
        &clients,
        &request_map,
        &cache,
        &last_active,
    )
    .await;

    assert_eq!(
        cache.lock().get("initialize"),
        Some(json!({"capabilities": {}}))
    );
    assert!(recovery_rx.try_recv().is_err());
}

#[tokio::test]
async fn tool_call_session_not_found_is_returned_to_client() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    request_map.lock().insert(
        "10".into(),
        pending_request("clientA", json!(77), Some("tools/call")),
    );
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_a = channel_client(&clients, "clientA");
    let last_active = last_active(None);

    let _recovery_rx = route_response(
        r#"{"jsonrpc":"2.0","id":10,"error":{"code":-32001,"message":"Session not found"}}"#,
        &clients,
        &request_map,
        &empty_cache(),
        &last_active,
    )
    .await;

    let routed: Value =
        serde_json::from_str(&rx_a.try_recv().expect("clientA receives concrete error"))
            .expect("valid json");
    assert_eq!(routed["id"], json!(77));
    assert_eq!(routed["error"]["message"], json!("Session not found"));
}

#[test]
fn classifier_helpers_distinguish_message_kinds() {
    let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
    assert!(message_has_method(&request));
    assert_eq!(non_null_id(&request), Some(&json!(1)));

    let notification = json!({"jsonrpc":"2.0","method":"notifications/progress"});
    assert!(message_has_method(&notification));
    assert_eq!(non_null_id(&notification), None);

    let response = json!({"jsonrpc":"2.0","id":7,"result":"ok"});
    assert!(!message_has_method(&response));
    assert_eq!(non_null_id(&response), Some(&json!(7)));

    let null_id = json!({"jsonrpc":"2.0","id":null,"method":"x"});
    assert_eq!(non_null_id(&null_id), None);
}

#[tokio::test]
async fn route_response_caches_success_not_error() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    request_map.lock().insert(
        "10".into(),
        pending_request("clientA", json!(1), Some("tools/list")),
    );
    request_map.lock().insert(
        "11".into(),
        pending_request("clientA", json!(2), Some("initialize")),
    );
    request_map.lock().insert(
        "12".into(),
        pending_request("clientA", json!(3), Some("tools/list")),
    );

    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let _rx_a = channel_client(&clients, "clientA");
    let cache = empty_cache();
    let last_active = last_active(None);

    route_response(
        r#"{"jsonrpc":"2.0","id":10,"result":{"tools":["t1"]}}"#,
        &clients,
        &request_map,
        &cache,
        &last_active,
    )
    .await;
    route_response(
        r#"{"jsonrpc":"2.0","id":11,"result":{"capabilities":{}}}"#,
        &clients,
        &request_map,
        &cache,
        &last_active,
    )
    .await;
    route_response(
        r#"{"jsonrpc":"2.0","id":12,"error":{"code":-32001,"message":"rate limited"}}"#,
        &clients,
        &request_map,
        &cache,
        &last_active,
    )
    .await;

    let guard = cache.lock();
    assert_eq!(
        guard.get("tools/list"),
        Some(json!({"tools":["t1"]})),
        "tools/list success cached"
    );
    assert_eq!(
        guard.get("initialize"),
        Some(json!({"capabilities":{}})),
        "initialize success cached"
    );
    assert_eq!(
        guard.get("tools/list"),
        Some(json!({"tools":["t1"]})),
        "error response not cached"
    );
}

#[tokio::test]
async fn route_response_does_not_cache_error_into_empty_cache() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    request_map.lock().insert(
        "20".into(),
        pending_request("clientA", json!(1), Some("tools/list")),
    );
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let _rx_a = channel_client(&clients, "clientA");
    let cache = empty_cache();
    let last_active = last_active(None);

    route_response(
        r#"{"jsonrpc":"2.0","id":20,"error":{"code":429,"message":"too many"}}"#,
        &clients,
        &request_map,
        &cache,
        &last_active,
    )
    .await;

    assert_eq!(cache.lock().get("tools/list"), None, "error not cached");
}

#[tokio::test]
async fn tools_list_cache_invalidated_on_list_changed() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_a = channel_client(&clients, "clientA");
    let last_active = last_active(None);

    let cache = empty_cache();
    cache.lock().store("tools/list", json!({"tools":["t1"]}));
    cache.lock().store("initialize", json!({"capabilities":{}}));

    route_response(
        r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#,
        &clients,
        &request_map,
        &cache,
        &last_active,
    )
    .await;

    assert_eq!(
        cache.lock().get("tools/list"),
        None,
        "tools/list invalidated"
    );
    assert_eq!(
        cache.lock().get("initialize"),
        Some(json!({"capabilities":{}})),
        "initialize not invalidated"
    );
    assert!(rx_a.try_recv().is_ok(), "list_changed still broadcast");
}

#[test]
fn cached_response_restored_with_new_client_id() {
    let cache = Arc::new(Mutex::new(HandshakeCache::default()));
    cache
        .lock()
        .store("tools/list", json!({"tools":["t1","t2"]}));

    let result = cache.lock().get("tools/list").expect("tools/list cached");
    let line = build_success_response(json!(99), result);
    let parsed: Value = serde_json::from_str(&line).expect("valid json");

    assert_eq!(parsed["jsonrpc"], json!("2.0"));
    assert_eq!(parsed["id"], json!(99), "new client's id stamped");
    assert_eq!(parsed["result"], json!({"tools":["t1","t2"]}));
    assert!(parsed.get("error").is_none(), "cached reply is a success");
}

#[tokio::test]
async fn only_eligible_discovery_success_is_cached() {
    let cache = empty_cache();
    let pending: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    let clients = Arc::new(Mutex::new(HashMap::new()));
    let _responses = channel_client(&clients, "clientA");
    let last_active = last_active(None);
    for (id, method, response, expected_cache) in [
        (
            1,
            "resources/list",
            json!({"jsonrpc":"2.0","id":1,"result":"x"}),
            None,
        ),
        (
            2,
            "tools/list",
            json!({"jsonrpc":"2.0","id":2,"result":{"tools":[]}}),
            Some(json!({"tools":[]})),
        ),
        (
            3,
            "tools/list",
            json!({"jsonrpc":"2.0","id":3,"error":{"code":-32001}}),
            Some(json!({"tools":[]})),
        ),
    ] {
        pending.lock().insert(
            jsonrpc::id_key(&json!(id)),
            pending_request("clientA", json!(id), Some(method)),
        );
        route_response(
            &response.to_string(),
            &clients,
            &pending,
            &cache,
            &last_active,
        )
        .await;
        assert_eq!(cache.lock().get("tools/list"), expected_cache);
    }
}

#[test]
fn concurrent_tools_list_misses_create_one_leader_and_one_waiter() {
    let cache = empty_cache();

    let first = prepare_tools_list_request(
        &cache,
        "clientA",
        json!(1),
        Duration::from_secs(REQUEST_TTL_SECS),
        Instant::now(),
    );
    let second = prepare_tools_list_request(
        &cache,
        "clientB",
        json!(2),
        Duration::from_secs(REQUEST_TTL_SECS),
        Instant::now(),
    );

    assert!(matches!(first, DiscoveryAction::Leader));
    assert!(matches!(second, DiscoveryAction::Coalesced));
    let guard = cache.lock();
    assert!(
        guard.tools_list.in_flight.is_some(),
        "leader owns upstream discovery"
    );
    assert_eq!(guard.tools_list.waiters.len(), 1, "follower waits");
    assert_eq!(guard.tools_list.waiters[0].client_id, "clientB");
}
