use super::initialize_tests::{Fixture, connect, read, send};
use super::*;
use serde_json::json;

pub(super) async fn reply(fixture: &Fixture, value: Value) -> io::Result<()> {
    fixture
        .responses
        .send(value.to_string())
        .await
        .map_err(io::Error::other)
}

pub(super) async fn client(fixture: &mut Fixture) -> io::Result<BufReader<LocalStream>> {
    let mut client = BufReader::new(connect(&fixture.proxy).await?);
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"connected","method":"ping"}),
    )
    .await?;
    let request = fixture.request().await?;
    reply(
        fixture,
        json!({"jsonrpc":"2.0","id":request.get("id"),"result":{}}),
    )
    .await?;
    assert_eq!(
        read(&mut client).await?.get("id"),
        Some(&json!("connected"))
    );
    Ok(client)
}

pub(super) async fn no_response(client: &mut BufReader<LocalStream>) {
    assert!(
        tokio::time::timeout(Duration::from_millis(50), client.fill_buf())
            .await
            .is_err()
    );
}

async fn initialize(
    fixture: &mut Fixture,
    client: &mut BufReader<LocalStream>,
    capabilities: Value,
) -> io::Result<()> {
    let cached = fixture.proxy.readiness().mcp_initialize_result_received;
    send(
        client.get_mut(),
        json!({
            "jsonrpc":"2.0","id":"init","method":"initialize","params":{"capabilities":capabilities}
        }),
    )
    .await?;
    if !cached {
        let request = fixture.request().await?;
        reply(
            fixture,
            json!({"jsonrpc":"2.0","id":request.get("id"),"result":{"capabilities":{}}}),
        )
        .await?;
    }
    assert_eq!(read(client).await?.get("id"), Some(&json!("init")));
    Ok(())
}

#[tokio::test]
async fn route_response_restores_ids_without_cross_wiring() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    let mut second = client(&mut fixture).await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":1,"method":"tools/call"}),
    )
    .await?;
    let first_request = fixture.request().await?;
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":"1","method":"tools/call"}),
    )
    .await?;
    let second_request = fixture.request().await?;
    assert_ne!(first_request.get("id"), second_request.get("id"));
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":second_request.get("id"),"result":"B"}),
    )
    .await?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":first_request.get("id"),"result":"A"}),
    )
    .await?;
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":1,"result":"A"})
    );
    assert_eq!(
        read(&mut second).await?,
        json!({"jsonrpc":"2.0","id":"1","result":"B"})
    );
    no_response(&mut first).await;
    no_response(&mut second).await;
    fixture.stop().await
}

#[tokio::test]
async fn route_response_routes_server_request_to_single_client() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    let mut second = client(&mut fixture).await?;
    initialize(&mut fixture, &mut first, json!({"sampling":{}})).await?;
    initialize(&mut fixture, &mut second, json!({"sampling":{}})).await?;
    let callback = json!({"jsonrpc":"2.0","id":"server-id","method":"sampling/createMessage"});
    reply(&fixture, callback.clone()).await?;
    assert_eq!(read(&mut first).await?, callback);
    no_response(&mut second).await;
    fixture.stop().await
}

async fn capable_callback(method: &str, capabilities: Value) -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    initialize(&mut fixture, &mut first, json!({})).await?;
    let mut second = client(&mut fixture).await?;
    initialize(&mut fixture, &mut second, capabilities).await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"active","method":"ping"}),
    )
    .await?;
    let active = fixture.request().await?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":active.get("id"),"result":{}}),
    )
    .await?;
    read(&mut first).await?;
    let callback = json!({"jsonrpc":"2.0","id":42,"method":method});
    reply(&fixture, callback.clone()).await?;
    assert_eq!(read(&mut second).await?, callback);
    no_response(&mut first).await;
    fixture.stop().await
}

#[tokio::test]
async fn sampling_server_request_routes_to_sampling_capable_client() -> io::Result<()> {
    capable_callback("sampling/createMessage", json!({"sampling":{}})).await
}

#[tokio::test]
async fn roots_server_request_routes_to_roots_capable_client() -> io::Result<()> {
    capable_callback("roots/list", json!({"roots":{}})).await
}

#[tokio::test]
async fn no_capable_client_sends_error_response_upstream() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    initialize(&mut fixture, &mut first, json!({})).await?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":44,"method":"sampling/createMessage"}),
    )
    .await?;
    assert_eq!(
        fixture.request().await?,
        json!({
            "jsonrpc":"2.0","id":44,"error":{
                "code":-32001,"message":"no capable downstream client connected for sampling/createMessage"
            }
        })
    );
    no_response(&mut first).await;
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}

#[tokio::test]
async fn route_response_broadcasts_notifications() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    let mut second = client(&mut fixture).await?;
    let notification = json!({"jsonrpc":"2.0","method":"notifications/progress"});
    reply(&fixture, notification.clone()).await?;
    assert_eq!(read(&mut first).await?, notification);
    assert_eq!(read(&mut second).await?, notification);
    fixture.stop().await
}

#[tokio::test]
async fn route_response_drops_when_origin_client_gone() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut origin = client(&mut fixture).await?;
    let mut other = client(&mut fixture).await?;
    send(
        origin.get_mut(),
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await?;
    let request = fixture.request().await?;
    drop(origin);
    tokio::time::timeout(Duration::from_secs(2), async {
        while fixture.proxy.connection_count() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":request.get("id"),"result":{"tools":[]}}),
    )
    .await?;
    no_response(&mut other).await;
    send(
        other.get_mut(),
        json!({"jsonrpc":"2.0","id":"cached","method":"tools/list"}),
    )
    .await?;
    assert_eq!(
        read(&mut other).await?,
        json!({"jsonrpc":"2.0","id":"cached","result":{"tools":[]}})
    );
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}

#[tokio::test]
async fn uncorrelated_errors_do_not_consume_pending_requests() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    let mut second = client(&mut fixture).await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":99,"method":"tools/call"}),
    )
    .await?;
    let request = fixture.request().await?;
    for error in [
        json!({"jsonrpc":"2.0","id":"","error":{"code":-32001,"message":"Session not found"}}),
        json!({"jsonrpc":"2.0","id":null,"error":{"code":-32001,"message":"Session not found"}}),
        json!({"jsonrpc":"2.0","error":{"code":-32001,"message":"Session not found"}}),
    ] {
        reply(&fixture, error).await?;
    }
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":request.get("id"),"result":"still routable"}),
    )
    .await?;
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":99,"result":"still routable"})
    );
    no_response(&mut second).await;
    fixture.stop().await
}

#[tokio::test]
async fn empty_id_success_is_dropped_as_orphan() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":99,"method":"tools/call"}),
    )
    .await?;
    let request = fixture.request().await?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":"","result":{"ok":true}}),
    )
    .await?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":request.get("id"),"result":"correct"}),
    )
    .await?;
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":99,"result":"correct"})
    );
    no_response(&mut first).await;
    fixture.stop().await
}

#[tokio::test]
async fn empty_id_error_without_pending_request_is_orphaned() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":"","error":{"code":-32001,"message":"Session not found"}}),
    )
    .await?;
    no_response(&mut first).await;
    assert!(fixture.proxy.readiness().upstream_transport_ready);
    fixture.stop().await
}
