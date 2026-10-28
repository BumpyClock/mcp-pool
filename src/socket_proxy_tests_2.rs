use super::initialize_tests::{Fixture, read, send};
use super::tests_1::{client, no_response, reply};
use super::*;
use serde_json::json;

async fn cache_result(
    fixture: &mut Fixture,
    client: &mut BufReader<LocalStream>,
    method: &str,
    result: Value,
) -> io::Result<()> {
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"seed","method":method}),
    )
    .await?;
    let request = fixture.request().await?;
    reply(
        fixture,
        json!({"jsonrpc":"2.0","id":request.get("id"),"result":result}),
    )
    .await?;
    assert_eq!(
        read(client).await?,
        json!({"jsonrpc":"2.0","id":"seed","result":result})
    );
    Ok(())
}

async fn cached_request(
    client: &mut BufReader<LocalStream>,
    method: &str,
    result: Value,
) -> io::Result<()> {
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"cached","method":method}),
    )
    .await?;
    assert_eq!(
        read(client).await?,
        json!({"jsonrpc":"2.0","id":"cached","result":result})
    );
    Ok(())
}

#[tokio::test]
async fn uncorrelated_session_error_preserves_caches_and_generation() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    cache_result(
        &mut fixture,
        &mut first,
        "initialize",
        json!({"capabilities":{}}),
    )
    .await?;
    cache_result(
        &mut fixture,
        &mut first,
        "tools/list",
        json!({"tools":["t1"]}),
    )
    .await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":99,"method":"tools/call"}),
    )
    .await?;
    let request = fixture.request().await?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":"","error":{"code":-32001,"message":"Session not found"}}),
    )
    .await?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":request.get("id"),"result":"retained"}),
    )
    .await?;
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":99,"result":"retained"})
    );
    cached_request(&mut first, "initialize", json!({"capabilities":{}})).await?;
    cached_request(&mut first, "tools/list", json!({"tools":["t1"]})).await?;
    assert!(fixture.requests.try_recv().is_err());
    assert!(fixture.proxy.readiness().upstream_transport_ready);
    fixture.stop().await
}

#[tokio::test]
async fn non_session_error_does_not_signal_recovery() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    cache_result(
        &mut fixture,
        &mut first,
        "initialize",
        json!({"capabilities":{}}),
    )
    .await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":99,"method":"tools/call"}),
    )
    .await?;
    let request = fixture.request().await?;
    reply(&fixture, json!({"jsonrpc":"2.0","id":request.get("id"),"error":{"code":-32001,"message":"rate limited"}})).await?;
    assert_eq!(
        read(&mut first).await?.pointer("/error/message"),
        Some(&json!("rate limited"))
    );
    cached_request(&mut first, "initialize", json!({"capabilities":{}})).await?;
    assert!(fixture.requests.try_recv().is_err());
    assert!(fixture.proxy.readiness().upstream_transport_ready);
    fixture.stop().await
}

#[tokio::test]
async fn tool_call_session_not_found_is_returned_to_client() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":77,"method":"tools/call"}),
    )
    .await?;
    let request = fixture.request().await?;
    reply(&fixture, json!({"jsonrpc":"2.0","id":request.get("id"),"error":{"code":-32001,"message":"Session not found"}})).await?;
    assert_eq!(
        read(&mut first).await?,
        json!({
            "jsonrpc":"2.0","id":77,"error":{"code":-32001,"message":"Session not found"}
        })
    );
    fixture.proxy.request_stop();
    fixture.stop().await
}

#[tokio::test]
async fn route_response_does_not_cache_error_into_empty_cache() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await?;
    let request = fixture.request().await?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":request.get("id"),"error":{"code":429,"message":"too many"}}),
    )
    .await?;
    assert_eq!(
        read(&mut first).await?.pointer("/error/code"),
        Some(&json!(429))
    );
    cache_result(&mut fixture, &mut first, "tools/list", json!({"tools":[]})).await?;
    fixture.stop().await
}

#[tokio::test]
async fn tools_list_cache_invalidated_on_list_changed() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    cache_result(
        &mut fixture,
        &mut first,
        "initialize",
        json!({"capabilities":{}}),
    )
    .await?;
    cache_result(
        &mut fixture,
        &mut first,
        "tools/list",
        json!({"tools":["t1"]}),
    )
    .await?;
    let changed = json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"});
    reply(&fixture, changed.clone()).await?;
    assert_eq!(read(&mut first).await?, changed);
    cached_request(&mut first, "initialize", json!({"capabilities":{}})).await?;
    cache_result(
        &mut fixture,
        &mut first,
        "tools/list",
        json!({"tools":["new"]}),
    )
    .await?;
    fixture.stop().await
}

#[tokio::test]
async fn cached_response_restored_with_new_client_id() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    cache_result(
        &mut fixture,
        &mut first,
        "tools/list",
        json!({"tools":["t1","t2"]}),
    )
    .await?;
    let mut second = client(&mut fixture).await?;
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":99,"method":"tools/list"}),
    )
    .await?;
    assert_eq!(
        read(&mut second).await?,
        json!({"jsonrpc":"2.0","id":99,"result":{"tools":["t1","t2"]}})
    );
    no_response(&mut first).await;
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}

#[tokio::test]
async fn only_eligible_discovery_success_is_cached() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = client(&mut fixture).await?;
    cache_result(
        &mut fixture,
        &mut first,
        "resources/list",
        json!({"resources":[]}),
    )
    .await?;
    cache_result(
        &mut fixture,
        &mut first,
        "resources/list",
        json!({"resources":["fresh"]}),
    )
    .await?;
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"tools-error","method":"tools/list"}),
    )
    .await?;
    let request = fixture.request().await?;
    reply(&fixture, json!({"jsonrpc":"2.0","id":request.get("id"),"error":{"code":-32001,"message":"discovery failed"}})).await?;
    assert_eq!(
        read(&mut first).await?.get("id"),
        Some(&json!("tools-error"))
    );
    cache_result(&mut fixture, &mut first, "tools/list", json!({"tools":[]})).await?;
    cached_request(&mut first, "tools/list", json!({"tools":[]})).await?;
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}
