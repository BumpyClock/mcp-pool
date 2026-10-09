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

// Build a last-active-client slot for route_response, optionally pre-set to a
// specific client (the heuristic target for server-initiated requests).
fn last_active(client_id: Option<&str>) -> Arc<Mutex<Option<String>>> {
    Arc::new(Mutex::new(client_id.map(str::to_string)))
}

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

async fn route_response_with_capabilities(
    line: &str,
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    client_capabilities: &Arc<Mutex<HashMap<String, ClientCapabilities>>>,
    request_tx: &Arc<Mutex<Option<mpsc::Sender<crate::upstream::UpstreamRequest>>>>,
) {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    let cache = empty_cache();
    let last_active = last_active(Some("clientA"));
    let (recovery_tx, _recovery_rx) = mpsc::channel::<RecoveryReason>(8);
    super::route_response(
        line,
        clients,
        &request_map,
        &cache,
        &last_active,
        client_capabilities,
        request_tx,
        &recovery_tx,
    )
    .await;
}

// The core multiplexing fix: two clients that independently used the same
// raw id (1) are tracked under distinct pool ids, so their responses route
// back to the right client with each client's original id restored — no
// cross-wiring, no broadcast.
#[tokio::test]
async fn route_response_restores_ids_without_cross_wiring() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    request_map
        .lock()
        .insert("1".into(), pending_request("clientA", json!(1), None));
    request_map
        .lock()
        .insert("2".into(), pending_request("clientB", json!(1), None));

    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_a = channel_client(&clients, "clientA");
    let mut rx_b = channel_client(&clients, "clientB");
    let cache = empty_cache();
    let last_active = last_active(None);

    route_response(
        r#"{"jsonrpc":"2.0","id":1,"result":"A"}"#,
        &clients,
        &request_map,
        &cache,
        &last_active,
    )
    .await;
    route_response(
        r#"{"jsonrpc":"2.0","id":2,"result":"B"}"#,
        &clients,
        &request_map,
        &cache,
        &last_active,
    )
    .await;

    let a: Value =
        serde_json::from_str(&rx_a.try_recv().expect("clientA response")).expect("valid json");
    let b: Value =
        serde_json::from_str(&rx_b.try_recv().expect("clientB response")).expect("valid json");
    assert_eq!(a["id"], json!(1), "clientA original id restored");
    assert_eq!(a["result"], json!("A"));
    assert_eq!(b["id"], json!(1), "clientB original id restored");
    assert_eq!(b["result"], json!("B"));

    // Each client received exactly one message: no cross-wiring.
    assert!(rx_a.try_recv().is_err());
    assert!(rx_b.try_recv().is_err());
}

// A server-initiated request (method + an id the pool never issued) must
// route to exactly ONE client — the heuristic last-active client — not
// broadcast. Broadcasting would make every client answer the same request,
// sending duplicate/conflicting responses upstream. The server's id is
// preserved verbatim so the client's response id matches.
#[tokio::test]
async fn route_response_routes_server_request_to_single_client() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_a = channel_client(&clients, "clientA");
    let mut rx_b = channel_client(&clients, "clientB");
    // clientA is the most-recently-active client → the heuristic target.
    let last_active = last_active(Some("clientA"));

    route_response(
        r#"{"jsonrpc":"2.0","id":42,"method":"sampling/createMessage"}"#,
        &clients,
        &request_map,
        &empty_cache(),
        &last_active,
    )
    .await;

    let received: Value =
        serde_json::from_str(&rx_a.try_recv().expect("clientA receives server request"))
            .expect("valid json");
    assert_eq!(received["id"], json!(42), "server id preserved verbatim");
    assert_eq!(received["method"], json!("sampling/createMessage"));
    // Exactly one client answers: no broadcast.
    assert!(rx_b.try_recv().is_err(), "server request not broadcast");
}

#[tokio::test]
async fn sampling_server_request_routes_to_sampling_capable_client() {
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_a = channel_client(&clients, "clientA");
    let mut rx_b = channel_client(&clients, "clientB");
    let capabilities = Arc::new(Mutex::new(HashMap::new()));
    capabilities.lock().insert(
        "clientA".to_string(),
        ClientCapabilities {
            sampling: false,
            roots: false,
        },
    );
    capabilities.lock().insert(
        "clientB".to_string(),
        ClientCapabilities {
            sampling: true,
            roots: false,
        },
    );
    let request_tx = Arc::new(Mutex::new(None));

    route_response_with_capabilities(
        r#"{"jsonrpc":"2.0","id":42,"method":"sampling/createMessage"}"#,
        &clients,
        &capabilities,
        &request_tx,
    )
    .await;

    assert!(rx_a.try_recv().is_err(), "incapable last-active not used");
    let received: Value =
        serde_json::from_str(&rx_b.try_recv().expect("sampling client receives request"))
            .expect("valid json");
    assert_eq!(received["method"], json!("sampling/createMessage"));
}

#[tokio::test]
async fn roots_server_request_routes_to_roots_capable_client() {
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_a = channel_client(&clients, "clientA");
    let mut rx_b = channel_client(&clients, "clientB");
    let capabilities = Arc::new(Mutex::new(HashMap::new()));
    capabilities.lock().insert(
        "clientA".to_string(),
        ClientCapabilities {
            sampling: true,
            roots: false,
        },
    );
    capabilities.lock().insert(
        "clientB".to_string(),
        ClientCapabilities {
            sampling: false,
            roots: true,
        },
    );
    let request_tx = Arc::new(Mutex::new(None));

    route_response_with_capabilities(
        r#"{"jsonrpc":"2.0","id":43,"method":"roots/list"}"#,
        &clients,
        &capabilities,
        &request_tx,
    )
    .await;

    assert!(rx_a.try_recv().is_err(), "roots-incapable client not used");
    let received: Value =
        serde_json::from_str(&rx_b.try_recv().expect("roots client receives request"))
            .expect("valid json");
    assert_eq!(received["method"], json!("roots/list"));
}

#[tokio::test]
async fn no_capable_client_sends_error_response_upstream() {
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_a = channel_client(&clients, "clientA");
    let capabilities = Arc::new(Mutex::new(HashMap::new()));
    capabilities.lock().insert(
        "clientA".to_string(),
        ClientCapabilities {
            sampling: false,
            roots: false,
        },
    );
    let (upstream_tx, mut upstream_rx) = mpsc::channel::<crate::upstream::UpstreamRequest>(8);
    let request_tx = Arc::new(Mutex::new(Some(upstream_tx)));

    route_response_with_capabilities(
        r#"{"jsonrpc":"2.0","id":44,"method":"sampling/createMessage"}"#,
        &clients,
        &capabilities,
        &request_tx,
    )
    .await;

    assert!(
        rx_a.try_recv().is_err(),
        "request not sent to incapable client"
    );
    let response: Value =
        serde_json::from_str(&upstream_rx.try_recv().expect("upstream receives error"))
            .expect("valid json");
    assert_eq!(response["id"], json!(44));
    assert_eq!(response["error"]["code"], json!(-32001));
    assert_eq!(
        response["error"]["message"],
        json!("no capable downstream client connected for sampling/createMessage")
    );
    assert!(upstream_rx.try_recv().is_err(), "single upstream error");
}

// A pure notification (method, no id) fans out to every client.
#[tokio::test]
async fn route_response_broadcasts_notifications() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_a = channel_client(&clients, "clientA");
    let mut rx_b = channel_client(&clients, "clientB");
    let last_active = last_active(None);

    route_response(
        r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#,
        &clients,
        &request_map,
        &empty_cache(),
        &last_active,
    )
    .await;
    assert!(rx_a.try_recv().is_ok(), "notification fans out to clientA");
    assert!(rx_b.try_recv().is_ok(), "notification fans out to clientB");
}

// A response whose originating client has disconnected is dropped, never
// rebroadcast (its restored id could collide with a live client's in-flight id).
#[tokio::test]
async fn route_response_drops_when_origin_client_gone() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    request_map
        .lock()
        .insert("5".into(), pending_request("ghost", json!(1), None));
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_other = channel_client(&clients, "other");
    let last_active = last_active(None);

    route_response(
        r#"{"jsonrpc":"2.0","id":5,"result":"X"}"#,
        &clients,
        &request_map,
        &empty_cache(),
        &last_active,
    )
    .await;

    assert!(
        rx_other.try_recv().is_err(),
        "orphan response not broadcast"
    );
}

#[tokio::test]
async fn uncorrelated_errors_do_not_consume_pending_requests() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    request_map.lock().insert(
        "10".into(),
        pending_request("clientA", json!(99), Some("tools/call")),
    );

    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_a = channel_client(&clients, "clientA");
    let mut rx_b = channel_client(&clients, "clientB");
    let last_active = last_active(None);

    for line in [
        r#"{"jsonrpc":"2.0","id":"","error":{"code":-32001,"message":"Session not found"}}"#,
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"Session not found"}}"#,
        r#"{"jsonrpc":"2.0","error":{"code":-32001,"message":"Session not found"}}"#,
    ] {
        route_response(line, &clients, &request_map, &empty_cache(), &last_active).await;
    }

    assert!(rx_a.try_recv().is_err(), "uncorrelated error not guessed");
    assert!(rx_b.try_recv().is_err(), "malformed error not broadcast");
    assert_eq!(request_map.lock().len(), 1, "pending request kept");
}

#[tokio::test]
async fn empty_id_success_is_dropped_as_orphan() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    request_map.lock().insert(
        "10".into(),
        pending_request("clientA", json!(99), Some("tools/call")),
    );

    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_a = channel_client(&clients, "clientA");
    let last_active = last_active(None);

    route_response(
        r#"{"jsonrpc":"2.0","id":"","result":{"ok":true}}"#,
        &clients,
        &request_map,
        &empty_cache(),
        &last_active,
    )
    .await;

    assert!(
        rx_a.try_recv().is_err(),
        "empty-id success not fallback routed"
    );
    assert_eq!(request_map.lock().len(), 1, "pending request kept");
}

#[tokio::test]
async fn empty_id_error_without_pending_request_is_orphaned() {
    let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
    let clients: Arc<Mutex<HashMap<String, ClientSender>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut rx_a = channel_client(&clients, "clientA");
    let last_active = last_active(None);

    route_response(
        r#"{"jsonrpc":"2.0","id":"","error":{"code":-32001,"message":"Session not found"}}"#,
        &clients,
        &request_map,
        &empty_cache(),
        &last_active,
    )
    .await;

    assert!(rx_a.try_recv().is_err(), "malformed error not broadcast");
}
