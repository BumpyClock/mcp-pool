use super::initialize_tests::{Fixture, connect, read, send};
use super::*;
use serde_json::json;

async fn respond(fixture: &Fixture, request: &Value, result: Value) -> io::Result<()> {
    fixture
        .responses
        .send(
            json!({
                "jsonrpc":"2.0", "id":request.get("id"), "result":result,
            })
            .to_string(),
        )
        .await
        .map_err(io::Error::other)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn different_cursors_stay_separate_and_do_not_overwrite_first_page_cache() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = connect(&fixture.proxy).await?;
    let mut second = connect(&fixture.proxy).await?;
    send(
        &mut first,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await?;
    let first_request = fixture.request().await?;
    let first_page = json!({"tools":[{"name":"first-page"}],"nextCursor":"alpha"});
    respond(&fixture, &first_request, first_page.clone()).await?;
    let mut first = BufReader::new(first);
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":1,"result":first_page})
    );

    let (first_write, second_write) = tokio::join!(
        send(
            first.get_mut(),
            json!({"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"cursor":"alpha"}})
        ),
        send(
            &mut second,
            json!({"jsonrpc":"2.0","id":7,"method":"tools/list","params":{"cursor":"beta"}})
        ),
    );
    first_write?;
    second_write?;
    let first_cursor = fixture.request().await?;
    let second_cursor = fixture.request().await?;
    assert_ne!(first_cursor.get("id"), second_cursor.get("id"));
    let alpha = json!({"tools":[{"name":"alpha-page"}],"nextCursor":"alpha-next"});
    let beta = json!({"tools":[{"name":"beta-page"}],"nextCursor":"beta-next"});
    for request in [&second_cursor, &first_cursor] {
        let result = match request
            .get("params")
            .and_then(|params| params.get("cursor"))
            .and_then(Value::as_str)
        {
            Some("alpha") => alpha.clone(),
            Some("beta") => beta.clone(),
            cursor => return Err(io::Error::other(format!("unexpected cursor {cursor:?}"))),
        };
        respond(&fixture, request, result).await?;
    }
    let mut second = BufReader::new(second);
    let (first_response, second_response) = tokio::join!(read(&mut first), read(&mut second));
    assert_eq!(
        first_response?,
        json!({"jsonrpc":"2.0","id":7,"result":alpha})
    );
    assert_eq!(
        second_response?,
        json!({"jsonrpc":"2.0","id":7,"result":beta})
    );
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":99,"method":"tools/list"}),
    )
    .await?;
    assert_eq!(
        read(&mut second).await?,
        json!({"jsonrpc":"2.0","id":99,"result":first_page})
    );
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_result_does_not_complete_first_page_leader_or_followers() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut page_client = connect(&fixture.proxy).await?;
    let mut leader_client = connect(&fixture.proxy).await?;
    send(
        &mut leader_client,
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    )
    .await?;
    let leader_request = fixture.request().await?;
    send(
        &mut page_client,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"cursor":"page-two"}}),
    )
    .await?;
    let cursor_request = fixture.request().await?;
    let mut follower_client = connect(&fixture.proxy).await?;
    send(
        &mut follower_client,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
    )
    .await?;
    let state = fixture
        .proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while state.handshake_cache.lock().tools_list.waiters.len() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    let second_page = json!({"tools":[{"name":"second-page"}]});
    respond(&fixture, &cursor_request, second_page.clone()).await?;
    let mut page_client = BufReader::new(page_client);
    assert_eq!(
        read(&mut page_client).await?,
        json!({"jsonrpc":"2.0","id":2,"result":second_page})
    );
    assert!(state.handshake_cache.lock().tools_list.in_flight);
    assert_eq!(state.handshake_cache.lock().tools_list.waiters.len(), 1);
    assert!(state.handshake_cache.lock().get("tools/list").is_none());
    let first_page = json!({"tools":[{"name":"first-page"}],"nextCursor":"page-two"});
    respond(&fixture, &leader_request, first_page.clone()).await?;
    let mut leader_client = BufReader::new(leader_client);
    let mut follower_client = BufReader::new(follower_client);
    let (leader_response, follower_response) =
        tokio::join!(read(&mut leader_client), read(&mut follower_client));
    assert_eq!(
        leader_response?,
        json!({"jsonrpc":"2.0","id":1,"result":first_page})
    );
    assert_eq!(
        follower_response?,
        json!({"jsonrpc":"2.0","id":3,"result":first_page})
    );
    assert_eq!(
        state.handshake_cache.lock().get("tools/list"),
        Some(first_page)
    );
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}
