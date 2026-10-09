use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use tokio::io::BufReader;

use super::tests::{fixture, read, reply, send};
use super::{McpClient, timeout_milliseconds};
use crate::request_deadline::TIMEOUT_FIELD;

#[tokio::test]
async fn initialize_carries_exact_timeout_without_transport_definition() -> Result<()> {
    let (stream, server) = tokio::io::duplex(4096);
    let mut server = BufReader::new(server);
    let server_work = async {
        let initialize = read(&mut server).await?;
        assert_eq!(initialize.get(TIMEOUT_FIELD), Some(&json!(2500)));
        assert_eq!(initialize.get("method"), Some(&json!("initialize")));
        reply(
            &mut server,
            &initialize,
            json!({"protocolVersion":"2025-06-18","capabilities":{}}),
        )
        .await?;
        let initialized = read(&mut server).await?;
        assert_eq!(
            initialized.get("method"),
            Some(&json!("notifications/initialized"))
        );
        assert_eq!(initialized.get(TIMEOUT_FIELD), None);
        Ok::<(), anyhow::Error>(())
    };
    let (result, served) = tokio::join!(
        McpClient::initialize(Box::new(stream), Duration::from_millis(2500)),
        server_work
    );
    served?;
    let client = result?;
    assert_eq!(client.deadline, Duration::from_millis(2500));
    Ok(())
}

#[tokio::test]
async fn local_requests_carry_over_five_minute_timeout_for_stdio_routing() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(600)).await?;
    let server_work = async {
        let request = read(&mut server).await?;
        assert_eq!(request.get(TIMEOUT_FIELD), Some(&json!(600000)));
        assert_eq!(
            request.get("params"),
            Some(&json!({"name":"example","arguments":{}}))
        );
        assert_eq!(request.get("id"), Some(&json!(2)));
        reply(&mut server, &request, json!({"content":[]})).await
    };
    let (result, served) = tokio::join!(
        client.request("tools/call", json!({"name":"example","arguments":{}})),
        server_work
    );
    served?;
    assert_eq!(result?, json!({"content":[]}));
    Ok(())
}

#[tokio::test]
async fn notifications_and_callback_responses_have_no_timeout_metadata() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    let server_work = async {
        let notification = read(&mut server).await?;
        assert_eq!(notification.get(TIMEOUT_FIELD), None);
        let request = read(&mut server).await?;
        assert_eq!(request.get(TIMEOUT_FIELD), Some(&json!(1000)));
        send(
            &mut server,
            json!({"jsonrpc":"2.0","id":"callback","method":"ping"}),
        )
        .await?;
        assert_eq!(
            read(&mut server).await?,
            json!({"jsonrpc":"2.0","id":"callback","result":{}})
        );
        reply(&mut server, &request, json!({})).await
    };
    let client_work = async {
        client
            .notify("notifications/message", json!({"data":"example"}))
            .await?;
        client.request("ping", json!({})).await?;
        Ok::<(), anyhow::Error>(())
    };
    let (result, served) = tokio::join!(client_work, server_work);
    served?;
    result
}

#[tokio::test]
async fn pagination_keeps_metadata_and_first_page_cursor_absent() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_millis(1000)).await?;
    let server_work = async {
        let first = read(&mut server).await?;
        assert_eq!(first.get(TIMEOUT_FIELD), Some(&json!(1000)));
        assert_eq!(first.pointer("/params/cursor"), None);
        reply(
            &mut server,
            &first,
            json!({"tools":[{"name":"first"}],"nextCursor":"second"}),
        )
        .await?;
        let second = read(&mut server).await?;
        assert_eq!(second.get(TIMEOUT_FIELD), Some(&json!(1000)));
        assert_eq!(second.pointer("/params/cursor"), Some(&json!("second")));
        reply(&mut server, &second, json!({"tools":[{"name":"second"}]})).await
    };
    let (result, served) = tokio::join!(client.list_tools(), server_work);
    served?;
    assert_eq!(
        result?,
        vec![json!({"name":"first"}), json!({"name":"second"})]
    );
    Ok(())
}

#[tokio::test]
async fn backend_hint_does_not_extend_local_deadline() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    client.deadline = Duration::from_millis(20);
    let server_work = async {
        let request = read(&mut server).await?;
        assert_eq!(request.get(TIMEOUT_FIELD), Some(&json!(20)));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(reply(&mut server, &request, json!({})).await.is_err());
        Ok::<(), anyhow::Error>(())
    };
    let (result, served) = tokio::join!(client.request("tools/call", json!({})), server_work);
    served?;
    let error = result.err().context("expected local timeout")?;
    assert!(error.to_string().contains("deadline exceeded"));
    assert!(client.reader.is_none());
    Ok(())
}

#[test]
fn timeout_metadata_rounds_up_and_rejects_zero_or_overflow() -> Result<()> {
    assert_eq!(timeout_milliseconds(Duration::from_nanos(1))?, 1);
    assert_eq!(timeout_milliseconds(Duration::from_micros(1001))?, 2);
    assert_eq!(timeout_milliseconds(Duration::from_millis(60000))?, 60000);
    assert_eq!(
        timeout_milliseconds(Duration::from_millis(u64::MAX))?,
        u64::MAX
    );
    assert!(timeout_milliseconds(Duration::ZERO).is_err());
    assert!(timeout_milliseconds(Duration::MAX).is_err());
    assert!(
        timeout_milliseconds(Duration::from_millis(u64::MAX) + Duration::from_nanos(1)).is_err()
    );
    Ok(())
}
