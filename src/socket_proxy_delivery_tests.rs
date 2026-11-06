use super::*;
use serde_json::json;
use std::future::{Future, poll_fn};
use std::pin::Pin;
use std::task::Poll;

fn fixture() -> (
    Arc<Generation>,
    mpsc::Receiver<String>,
    mpsc::Receiver<String>,
) {
    let generation = Arc::new(Generation::new(diagnostics::ServerLog::default()));
    let (leader_sender, leader) = mpsc::channel(1);
    let (follower_sender, follower) = mpsc::channel(1);
    generation
        .clients
        .lock()
        .insert("leader".into(), leader_sender);
    generation
        .clients
        .lock()
        .insert("follower".into(), follower_sender);
    (generation, leader, follower)
}

fn fill(generation: &Generation, client: &str) -> io::Result<()> {
    generation
        .clients
        .lock()
        .get(client)
        .ok_or_else(|| io::Error::other("missing client"))?
        .try_send("occupied".into())
        .map_err(io::Error::other)
}

fn seed(generation: &Generation, method: CacheableMethod) -> io::Result<u64> {
    let inserted_at = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .ok_or_else(|| io::Error::other("clock"))?;
    let method_name = match method {
        CacheableMethod::Initialize => "initialize",
        CacheableMethod::ServerDiscover => "server/discover",
        CacheableMethod::ToolsList => "tools/list",
    };
    let mut pool_id = None;
    for (client, id, leader) in [
        ("leader", "leader-id", true),
        ("follower", "follower-id", false),
    ] {
        let action = generation.routes.admit(
            PendingRequestInfo {
                client_id: client.into(),
                original_id: json!(id),
                method: Some(method_name.into()),
                cache_key: Some(method),
                tool: None,
                inserted_at,
                expires_after: Duration::ZERO,
            },
            inserted_at,
        );
        if leader {
            let RouteAdmission::Leader(identifier) = action else {
                return Err(io::Error::other("expected shared leader"));
            };
            pool_id = Some(identifier);
        } else {
            assert!(matches!(action, RouteAdmission::Coalesced));
        }
    }
    pool_id.ok_or_else(|| io::Error::other("missing leader"))
}

async fn poll_pending<F: Future>(mut future: Pin<&mut F>) {
    poll_fn(|context| {
        assert!(future.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
}

#[tokio::test]
async fn shared_completion_reaches_ready_follower_before_leader_unblocks() -> io::Result<()> {
    for method in [
        CacheableMethod::Initialize,
        CacheableMethod::ServerDiscover,
        CacheableMethod::ToolsList,
    ] {
        let (generation, mut leader, mut follower) = fixture();
        let pool_id = seed(&generation, method)?;
        fill(&generation, "leader")?;
        let (recovery, _receiver) = mpsc::channel(8);
        let response = json!({"jsonrpc":"2.0","id":pool_id,"result":{"supportedVersions":["2026-07-28"],"capabilities":{},"tools":[]}}).to_string();
        let routing = route_response(&response, &generation, &recovery);
        tokio::pin!(routing);
        poll_pending(routing.as_mut()).await;
        let value: Value = serde_json::from_str(&follower.try_recv().map_err(io::Error::other)?)?;
        assert_eq!(value.get("id"), Some(&json!("follower-id")));
        assert_eq!(leader.try_recv().map_err(io::Error::other)?, "occupied");
        routing.await;
        let value: Value = serde_json::from_str(&leader.try_recv().map_err(io::Error::other)?)?;
        assert_eq!(value.get("id"), Some(&json!("leader-id")));
        assert!(follower.try_recv().is_err());
    }
    Ok(())
}

#[tokio::test]
async fn broadcast_reaches_ready_recipient_before_blocked_recipient_unblocks() -> io::Result<()> {
    let (generation, mut leader, mut follower) = fixture();
    let blocked = generation
        .clients
        .lock()
        .keys()
        .next()
        .cloned()
        .ok_or_else(|| io::Error::other("missing clients"))?;
    fill(&generation, &blocked)?;
    let notification = json!({"jsonrpc":"2.0","method":"notifications/progress"}).to_string();
    let broadcasting =
        broadcast_to_all(&notification, &generation.clients, &generation.diagnostics);
    tokio::pin!(broadcasting);
    poll_pending(broadcasting.as_mut()).await;
    let (ready, full) = if blocked == "leader" {
        (&mut follower, &mut leader)
    } else {
        (&mut leader, &mut follower)
    };
    assert_eq!(ready.try_recv().map_err(io::Error::other)?, notification);
    assert_eq!(full.try_recv().map_err(io::Error::other)?, "occupied");
    broadcasting.await;
    assert_eq!(full.try_recv().map_err(io::Error::other)?, notification);
    assert!(ready.try_recv().is_err());
    Ok(())
}

#[tokio::test]
async fn expiration_reaches_ready_follower_before_leader_delivery_wait() -> io::Result<()> {
    let (generation, mut leader, mut follower) = fixture();
    seed(&generation, CacheableMethod::Initialize)?;
    fill(&generation, "leader")?;
    let expiring = generation::expiration_loop(generation.clone());
    tokio::pin!(expiring);
    poll_fn(|context| {
        assert!(expiring.as_mut().poll(context).is_pending());
        if generation.routes.pending_count() == 0 {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
    let value: Value = serde_json::from_str(&follower.try_recv().map_err(io::Error::other)?)?;
    assert_eq!(value.get("id"), Some(&json!("follower-id")));
    assert_eq!(leader.try_recv().map_err(io::Error::other)?, "occupied");
    generation.signal_shutdown();
    expiring.await;
    Ok(())
}

#[tokio::test]
async fn same_client_order_and_next_message_barrier() -> io::Result<()> {
    let (generation, mut leader, mut follower) = fixture();
    let pool_id = seed(&generation, CacheableMethod::Initialize)?;
    let now = Instant::now();
    let action = generation.routes.admit(
        PendingRequestInfo {
            client_id: "leader".into(),
            original_id: json!("second"),
            method: Some("initialize".into()),
            cache_key: Some(CacheableMethod::Initialize),
            tool: None,
            inserted_at: now,
            expires_after: Duration::from_secs(30),
        },
        now,
    );
    assert!(matches!(action, RouteAdmission::Coalesced));
    fill(&generation, "leader")?;
    let (recovery, _receiver) = mpsc::channel(8);
    let response = json!({"jsonrpc":"2.0","id":pool_id,"error":{"code":-32603,"message":"failed"}})
        .to_string();
    let notification = json!({"jsonrpc":"2.0","method":"notifications/progress"}).to_string();
    let route = async {
        route_response(&response, &generation, &recovery).await;
        broadcast_to_all(&notification, &generation.clients, &generation.diagnostics).await;
    };
    tokio::pin!(route);
    poll_pending(route.as_mut()).await;
    let value: Value = serde_json::from_str(&follower.try_recv().map_err(io::Error::other)?)?;
    assert_eq!(value.get("id"), Some(&json!("follower-id")));
    assert!(follower.try_recv().is_err());
    assert_eq!(leader.try_recv().map_err(io::Error::other)?, "occupied");
    poll_pending(route.as_mut()).await;
    let first: Value = serde_json::from_str(&leader.try_recv().map_err(io::Error::other)?)?;
    assert_eq!(first.get("id"), Some(&json!("leader-id")));
    poll_pending(route.as_mut()).await;
    let second: Value = serde_json::from_str(&leader.try_recv().map_err(io::Error::other)?)?;
    assert_eq!(second.get("id"), Some(&json!("second")));
    route.await;
    assert_eq!(leader.try_recv().map_err(io::Error::other)?, notification);
    assert_eq!(follower.try_recv().map_err(io::Error::other)?, notification);
    Ok(())
}

#[tokio::test]
async fn orphan_and_closed_recipients_do_not_redirect() -> io::Result<()> {
    for closed in [false, true] {
        let (generation, leader, mut follower) = fixture();
        let pool_id = seed(&generation, CacheableMethod::Initialize)?;
        drop(leader);
        if !closed {
            generation.clients.lock().remove("leader");
        }
        let (recovery, mut recovery_receiver) = mpsc::channel(8);
        let response =
            json!({"jsonrpc":"2.0","id":pool_id,"error":{"code":-32603,"message":"failed"}})
                .to_string();
        route_response(&response, &generation, &recovery).await;
        let value: Value = serde_json::from_str(&follower.try_recv().map_err(io::Error::other)?)?;
        assert_eq!(value.get("id"), Some(&json!("follower-id")));
        assert_eq!(value.pointer("/error/message"), Some(&json!("failed")));
        assert!(follower.try_recv().is_err());
        assert!(recovery_receiver.try_recv().is_err());
    }
    Ok(())
}
