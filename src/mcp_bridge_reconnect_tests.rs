use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use super::super::{BridgeClient, BridgeState, ClientFuture, ReconnectFactory, rpc};
use super::{SyntheticClient, tool};
use crate::config::ServerDef;

#[tokio::test]
async fn timed_out_call_is_not_replayed_and_next_list_and_call_reconnect() -> Result<()> {
    let calls = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let reconnects = Arc::new(AtomicUsize::new(0));
    let reconnect_calls = Arc::clone(&calls);
    let reconnect_count = Arc::clone(&reconnects);
    let reconnect: ReconnectFactory = Arc::new(move || {
        reconnect_count.fetch_add(1, Ordering::SeqCst);
        let calls = Arc::clone(&reconnect_calls);
        Box::pin(async move {
            Ok(Box::new(SyntheticClient {
                server: "alpha".to_owned(),
                tools: vec![tool("echo", "echo")],
                calls,
                closed: Arc::new(AtomicBool::new(false)),
                active: None,
                maximum: None,
            }) as Box<dyn BridgeClient>)
        }) as ClientFuture<'static, Box<dyn BridgeClient>>
    });
    let initial_client = SyntheticClient {
        server: "alpha".to_owned(),
        tools: vec![tool("echo", "echo")],
        calls: Arc::clone(&calls),
        closed: Arc::new(AtomicBool::new(false)),
        active: None,
        maximum: None,
    };
    let state = BridgeState::with_reconnectable_clients(vec![(
        "alpha".to_owned(),
        Box::new(initial_client),
        reconnect,
        Duration::from_millis(5),
    )])?;

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
    assert_eq!(reconnects.load(Ordering::SeqCst), 0);

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
    assert_eq!(reconnects.load(Ordering::SeqCst), 1);
    assert_eq!(
        calls.lock().await.as_slice(),
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
