use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Fixture, rpc, test_fixture, tool};

#[tokio::test]
async fn large_call_arguments_results_and_numeric_ids_survive_dispatch() -> Result<()> {
    let fixture = Fixture::new("alpha", vec![tool("lookup", "lookup")]);
    let state = test_fixture::default_state(vec![fixture.clone()]).await?;
    let arguments = json!({"text":"x".repeat(512 * 1024),"nested":[{"value":null},[1,2,3]]});
    for id in [json!("client"), json!(-1), json!(u64::MAX), json!(1.25)] {
        let response = rpc::dispatch(
            &state,
            json!({
                "jsonrpc":"2.0","id":id,"method":"tools/call",
                "params":{"name":"alpha__lookup","arguments":arguments}
            }),
            None,
        )
        .await
        .context("missing response")?;
        assert_eq!(response.get("id"), Some(&id));
        assert_eq!(response.pointer("/result/echo/arguments"), Some(&arguments));
    }
    let calls = fixture.calls().await;
    assert_eq!(calls.len(), 4);
    for call in calls {
        assert_eq!(call.pointer("/params/name"), Some(&json!("lookup")));
        assert_eq!(call.pointer("/params/arguments"), Some(&arguments));
    }
    Ok(())
}

#[tokio::test]
async fn request_validation_preserves_ids_errors_and_notification_acknowledgements() -> Result<()> {
    let fixture = Fixture::new("alpha", vec![tool("lookup", "lookup")]);
    let state = test_fixture::default_state(vec![fixture.clone()]).await?;
    for (message, id, code, error) in [
        (json!([]), Value::Null, -32600, "Invalid JSON-RPC request."),
        (
            json!({"id":true,"jsonrpc":"wrong","method":null}),
            Value::Null,
            -32600,
            "Invalid JSON-RPC request id.",
        ),
        (
            json!({"id":"bad-version","jsonrpc":"wrong","method":null}),
            json!("bad-version"),
            -32600,
            "Invalid JSON-RPC version.",
        ),
        (
            json!({"jsonrpc":"2.0","id":1.25,"method":"","params":null}),
            json!(1.25),
            -32600,
            "Invalid JSON-RPC method.",
        ),
        (
            json!({"jsonrpc":"2.0","id":2,"method":"ping","params":null}),
            json!(2),
            -32600,
            "Invalid JSON-RPC parameters.",
        ),
        (
            json!({"jsonrpc":"2.0","method":"notifications/initialized","params":null}),
            Value::Null,
            -32600,
            "Invalid JSON-RPC parameters.",
        ),
        (
            json!({"jsonrpc":"2.0","id":3,"method":"notifications/initialized"}),
            json!(3),
            -32600,
            "Notifications must not include a request id.",
        ),
        (
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":[]}),
            json!(4),
            -32602,
            "Invalid MCP bridge parameters.",
        ),
        (
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"alpha__lookup","arguments":null}}),
            json!(5),
            -32602,
            "Invalid MCP bridge parameters.",
        ),
    ] {
        let response = rpc::dispatch(&state, message, None)
            .await
            .context("missing error")?;
        assert_eq!(
            response,
            json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":error}})
        );
    }
    for message in [
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":null,"method":"notifications/initialized","params":[]}),
        json!({"jsonrpc":"2.0","method":"unknown","params":{}}),
    ] {
        assert_eq!(rpc::dispatch(&state, message, None).await, None);
    }
    for params in [None, Some(json!([])), Some(json!({}))] {
        let mut message = json!({"jsonrpc":"2.0","id":7,"method":"ping"});
        if let Some(params) = params {
            message
                .as_object_mut()
                .context("request object")?
                .insert("params".to_owned(), params);
        }
        assert_eq!(
            rpc::dispatch(&state, message, None).await,
            Some(json!({"jsonrpc":"2.0","id":7,"result":{}}))
        );
    }
    assert!(fixture.calls().await.is_empty());
    Ok(())
}

#[test]
fn subscription_validation_preserves_request_ids_and_error_classes() -> Result<()> {
    for (message, id, code) in [
        (
            json!({"jsonrpc":"2.0","method":"subscriptions/listen","params":{"notifications":{}}}),
            Value::Null,
            -32600,
        ),
        (
            json!({"jsonrpc":"2.0","id":null,"method":"subscriptions/listen","params":{"notifications":{}}}),
            Value::Null,
            -32600,
        ),
        (
            json!({"jsonrpc":"2.0","id":1.25,"method":"subscriptions/listen","params":[]}),
            json!(1.25),
            -32602,
        ),
        (
            json!({"jsonrpc":"2.0","id":"bad","method":"subscriptions/listen","params":null}),
            json!("bad"),
            -32600,
        ),
        (
            json!({"jsonrpc":"2.0","id":-1,"method":"subscriptions/listen","params":{"notifications":null}}),
            json!(-1),
            -32602,
        ),
        (
            json!({"jsonrpc":"2.0","id":u64::MAX,"method":"subscriptions/listen","params":{"notifications":{"toolsListChanged":false,"unknown":"bad"}}}),
            json!(u64::MAX),
            -32602,
        ),
    ] {
        let response = rpc::listen_subscription(message)
            .err()
            .context("expected listener error")?;
        assert_eq!(response.get("id"), Some(&id));
        assert_eq!(response.pointer("/error/code"), Some(&json!(code)));
    }
    for filter in [
        json!({}),
        json!({"toolsListChanged":false}),
        json!({"toolsListChanged":true,"unknown":false}),
    ] {
        let listener = rpc::listen_subscription(json!({
            "jsonrpc":"2.0","id":"listener","method":"subscriptions/listen",
            "params":{"notifications":filter,"extension":{"text":"x".repeat(256 * 1024)}}
        }))
        .map_err(|error| anyhow::anyhow!("{error}"))?;
        assert_eq!(listener.id, json!("listener"));
        assert_eq!(
            listener.tools_list_changed,
            filter.get("toolsListChanged") == Some(&json!(true))
        );
    }
    Ok(())
}
