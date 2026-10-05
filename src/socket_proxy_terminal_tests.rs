use super::initialize_tests::{Fixture, read, two_clients};
use super::*;
use serde_json::json;

async fn exact_response_then_eof(expected_first: Value, expected_second: Value) -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let (mut first, mut second, leader) = two_clients(&mut fixture).await?;
    let mut upstream = expected_first.clone();
    let upstream_object = upstream
        .as_object_mut()
        .ok_or_else(|| io::Error::other("fixture response must be an object"))?;
    upstream_object.insert(
        "id".to_string(),
        leader
            .get("id")
            .cloned()
            .ok_or_else(|| io::Error::other("upstream request needs an id"))?,
    );
    fixture
        .responses
        .send(upstream.to_string())
        .await
        .map_err(io::Error::other)?;
    fixture.retired.send_replace(Some(Ok(())));
    let (first_response, second_response) = tokio::join!(read(&mut first), read(&mut second));
    assert_eq!(first_response?, expected_first);
    assert_eq!(second_response?, expected_second);
    let mut first_line = String::new();
    let mut second_line = String::new();
    let eof = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(
            first.read_line(&mut first_line),
            second.read_line(&mut second_line)
        )
    })
    .await
    .map_err(io::Error::other)?;
    assert_eq!(eof.0?, 0);
    assert_eq!(eof.1?, 0);
    fixture.stop().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_backend_response_reaches_initialize_followers_before_disconnect() -> io::Result<()>
{
    exact_response_then_eof(
        json!({"jsonrpc":"2.0","id":1,"error":{
            "code":-32602,"message":"initialize failed before transport closed"
        }}),
        json!({"jsonrpc":"2.0","id":"second","error":{
            "code":-32602,"message":"initialize failed before transport closed"
        }}),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_terminal_response_reaches_initialize_followers_before_eof() -> io::Result<()> {
    exact_response_then_eof(
        json!({"jsonrpc":"2.0","id":1,"result":{
            "protocolVersion":"2025-03-26","capabilities":{}
        }}),
        json!({"jsonrpc":"2.0","id":"second","result":{
            "protocolVersion":"2025-03-26","capabilities":{}
        }}),
    )
    .await
}
