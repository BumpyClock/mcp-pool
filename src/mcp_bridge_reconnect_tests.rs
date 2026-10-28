use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use super::super::{ReconnectFactory, filter, rpc, test_fixture};
use super::{Fixture, tool};
use crate::config::ServerDef;

#[tokio::test]
async fn timed_out_call_is_not_replayed_and_next_list_and_call_reconnect() -> Result<()> {
    let fixture = Fixture::new("alpha", vec![tool("echo", "echo")]);
    let state = test_fixture::state(vec![(
        fixture.clone(),
        filter::ToolFilter::default(),
        Duration::from_millis(150),
    )])
    .await?;

    let timeout_response = rpc::dispatch(
        &state,
        json!({
            "jsonrpc":"2.0",
            "id":"timeout-call",
            "method":"tools/call",
            "params":{"name":"alpha__echo","arguments":{"label":"timeout"}}
        }),
        None,
    )
    .await;
    assert_eq!(
        timeout_response
            .as_ref()
            .and_then(|message| message.get("id")),
        Some(&json!("timeout-call"))
    );
    assert_eq!(
        timeout_response
            .as_ref()
            .and_then(|message| message.pointer("/error/code")),
        Some(&json!(-32603))
    );
    fixture.wait_for(1, 1, 1).await?;
    assert_eq!(fixture.connections(), 1);

    let list_response = rpc::dispatch(
        &state,
        json!({
            "jsonrpc":"2.0",
            "id":"list-after-timeout",
            "method":"tools/list",
            "params":{}
        }),
        None,
    )
    .await;
    assert_eq!(
        list_response.as_ref().and_then(|message| message.get("id")),
        Some(&json!("list-after-timeout"))
    );
    assert_eq!(
        list_response
            .as_ref()
            .and_then(|message| message.pointer("/result/tools/0/name")),
        Some(&json!("alpha__echo"))
    );

    let echo_response = rpc::dispatch(
        &state,
        json!({
            "jsonrpc":"2.0",
            "id":"echo-after-timeout",
            "method":"tools/call",
            "params":{"name":"alpha__echo","arguments":{"label":"after"}}
        }),
        None,
    )
    .await;
    assert_eq!(
        echo_response.as_ref().and_then(|message| message.get("id")),
        Some(&json!("echo-after-timeout"))
    );
    assert_eq!(
        echo_response
            .as_ref()
            .and_then(|message| message.pointer("/result/echo/arguments/label")),
        Some(&json!("after"))
    );
    assert_eq!(fixture.connections(), 2);
    assert_eq!(
        fixture.calls().await.as_slice(),
        &[
            json!({
                "server":"alpha",
                "method":"tools/call",
                "params":{"name":"echo","arguments":{"label":"timeout"}}
            }),
            json!({
                "server":"alpha",
                "method":"tools/call",
                "params":{"name":"echo","arguments":{"label":"after"}}
            })
        ]
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_call_closes_connection_and_next_call_does_not_replay() -> Result<()> {
    let fixture = Fixture::new("alpha", vec![tool("echo", "echo")]);
    let state = Arc::new(test_fixture::default_state(vec![fixture.clone()]).await?);
    let active_state = Arc::clone(&state);
    let active = tokio::spawn(async move {
        active_state
            .call_tool("alpha__echo", json!({"label":"hold"}), None)
            .await
    });
    fixture.wait_for(1, 0, 1).await?;
    active.abort();
    assert!(active.await.is_err());
    fixture.wait_for(1, 1, 1).await?;
    let response = state
        .call_tool("alpha__echo", json!({"label":"after"}), None)
        .await
        .map_err(|_| anyhow::anyhow!("call after cancellation failed"))?;
    assert_eq!(
        response.pointer("/echo/arguments/label"),
        Some(&json!("after"))
    );
    assert_eq!(fixture.connections(), 2);
    let calls = fixture.calls().await;
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls
            .first()
            .and_then(|call| call.pointer("/params/arguments/label")),
        Some(&json!("hold"))
    );
    assert_eq!(
        calls
            .get(1)
            .and_then(|call| call.pointer("/params/arguments/label")),
        Some(&json!("after"))
    );
    Ok(())
}

#[tokio::test]
async fn queue_wait_counts_toward_call_timeout_and_calls_are_serialized() -> Result<()> {
    let fixture = Fixture::new("alpha", vec![tool("echo", "echo")]);
    let state = Arc::new(
        test_fixture::state(vec![(
            fixture.clone(),
            filter::ToolFilter::default(),
            Duration::from_millis(800),
        )])
        .await?,
    );
    let first_state = Arc::clone(&state);
    let first = tokio::spawn(async move {
        first_state
            .call_tool("alpha__echo", json!({"label":"hold"}), None)
            .await
    });
    fixture.wait_for(1, 0, 1).await?;
    let second_state = Arc::clone(&state);
    let second = tokio::spawn(async move {
        second_state
            .call_tool("alpha__echo", json!({"label":"timeout"}), None)
            .await
    });
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(fixture.calls().await.len(), 1);
    fixture.release_call();
    assert!(first.await?.is_ok());
    fixture.wait_for(1, 0, 2).await?;
    let second = tokio::time::timeout(Duration::from_millis(650), second).await??;
    assert!(second.is_err());
    fixture.wait_for(1, 1, 2).await?;
    assert_eq!(fixture.connections(), 1);
    Ok(())
}

#[tokio::test]
async fn reconnect_wait_counts_toward_call_timeout_without_replay() -> Result<()> {
    let fixture = Fixture::new("alpha", vec![tool("echo", "echo")]);
    let mut state = test_fixture::state(vec![(
        fixture.clone(),
        filter::ToolFilter::default(),
        Duration::from_millis(150),
    )])
    .await?;
    let attempts = Arc::new(AtomicUsize::new(0));
    let reconnect_fixture = fixture.clone();
    let reconnect_attempts = Arc::clone(&attempts);
    let reconnect: ReconnectFactory = Arc::new(move || {
        let fixture = reconnect_fixture.clone();
        let attempt = reconnect_attempts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if attempt == 0 {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            fixture.connect().await
        })
    });
    state
        .clients
        .get_mut("alpha")
        .ok_or_else(|| anyhow::anyhow!("missing fixture server"))?
        .reconnect = reconnect;
    let state = Arc::new(state);
    let first = state
        .call_tool("alpha__echo", json!({"label":"timeout"}), None)
        .await;
    assert!(first.is_err());
    fixture.wait_for(1, 1, 1).await?;
    let waiting = state
        .call_tool("alpha__echo", json!({"label":"not-sent"}), None)
        .await;
    assert!(waiting.is_err());
    assert_eq!(fixture.connections(), 1);
    assert_eq!(fixture.calls().await.len(), 1);
    let response = state
        .call_tool("alpha__echo", json!({"label":"after"}), None)
        .await
        .map_err(|_| anyhow::anyhow!("call after reconnect failed"))?;
    assert_eq!(
        response.pointer("/echo/arguments/label"),
        Some(&json!("after"))
    );
    assert_eq!(fixture.connections(), 2);
    assert_eq!(fixture.calls().await.len(), 2);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    Ok(())
}

#[test]
fn configured_server_timeout_is_not_capped() {
    let definition = ServerDef {
        timeout_ms: Some(300_000),
        ..ServerDef::default()
    };
    assert_eq!(
        super::super::request_timeout(&definition),
        Duration::from_millis(300_000)
    );
}
