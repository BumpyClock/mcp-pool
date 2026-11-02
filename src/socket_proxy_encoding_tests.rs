use super::initialize_tests::{Fixture, connect, read, send};
use super::tests_1::reply;
use super::*;
use serde_json::json;

#[tokio::test]
async fn stdio_identified_requests_strip_metadata_and_restore_distinct_ids() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = BufReader::new(connect(&fixture.proxy).await?);
    let mut second = BufReader::new(connect(&fixture.proxy).await?);
    let arguments = json!({
        "document":"x".repeat(256 * 1024),
        "escaped":"quote \" slash \\ newline \n",
        "_mcp_pool_timeout_ms":"nested tool argument"
    });
    send(
        first.get_mut(),
        json!({
            "jsonrpc":"2.0","id":7,"method":"tools/call",
            "params":{"name":"inspect","arguments":arguments},
            "_mcp_pool_timeout_ms":600_000
        }),
    )
    .await?;
    let first_request = fixture.request().await?;
    send(
        second.get_mut(),
        json!({
            "jsonrpc":"2.0","id":"7","method":"tools/call",
            "params":{"name":"inspect","arguments":arguments},
            "_mcp_pool_timeout_ms":600_000
        }),
    )
    .await?;
    let second_request = fixture.request().await?;
    for request in [&first_request, &second_request] {
        assert!(request.get("_mcp_pool_timeout_ms").is_none());
        assert_eq!(request.get("jsonrpc"), Some(&json!("2.0")));
        assert_eq!(request.get("method"), Some(&json!("tools/call")));
        assert_eq!(
            request.get("params"),
            Some(&json!({"name":"inspect","arguments":arguments}))
        );
        assert!(
            request
                .get("id")
                .and_then(Value::as_u64)
                .is_some_and(|identifier| identifier != 7)
        );
    }
    assert_ne!(first_request.get("id"), second_request.get("id"));
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":second_request.get("id"),"result":"second"}),
    )
    .await?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":first_request.get("id"),"result":"first"}),
    )
    .await?;
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":7,"result":"first"})
    );
    assert_eq!(
        read(&mut second).await?,
        json!({"jsonrpc":"2.0","id":"7","result":"second"})
    );
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}

#[tokio::test]
async fn stdio_unmodified_raw_messages_preserve_bytes_and_input_order() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut client = connect(&fixture.proxy).await?;
    let messages = [
        r#" { "jsonrpc" : "2.0", "method" : "notifications/progress", "params" : {} } "#,
        r#"{"jsonrpc":"2.0", "id":null, "method":"ping"}"#,
        r#" { "jsonrpc":"2.0", "id":"server-id", "result":1.00e+2 } "#,
        r#"{"jsonrpc":"2.0", "id":42, "error":{"code":-1,"message":"failed"}}"#,
        r#"[ 1, {"_mcp_pool_timeout_ms":0}, null ]"#,
        r#""text with spaces""#,
        r#" { malformed : json "#,
    ];
    client
        .write_all(format!("{}\n", messages.join("\n")).as_bytes())
        .await?;
    for expected in messages {
        let forwarded = tokio::time::timeout(Duration::from_secs(2), fixture.requests.recv())
            .await
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("request channel closed"))?;
        assert_eq!(forwarded.line, expected);
    }
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}

#[tokio::test]
async fn stdio_modified_raw_messages_strip_only_top_level_metadata() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut client = connect(&fixture.proxy).await?;
    for (input, expected) in [
        (
            json!({
                "jsonrpc":"2.0","method":"notifications/progress",
                "params":{"_mcp_pool_timeout_ms":123},"_mcp_pool_timeout_ms":600_000
            }),
            json!({
                "jsonrpc":"2.0","method":"notifications/progress",
                "params":{"_mcp_pool_timeout_ms":123}
            }),
        ),
        (
            json!({"jsonrpc":"2.0","id":null,"method":"ping","_mcp_pool_timeout_ms":600_000}),
            json!({"jsonrpc":"2.0","id":null,"method":"ping"}),
        ),
        (
            json!({"jsonrpc":"2.0","id":null,"result":{},"_mcp_pool_timeout_ms":600_000}),
            json!({"jsonrpc":"2.0","id":null,"result":{}}),
        ),
        (
            json!({"jsonrpc":"2.0","result":{},"_mcp_pool_timeout_ms":600_000}),
            json!({"jsonrpc":"2.0","result":{}}),
        ),
    ] {
        send(&mut client, input).await?;
        assert_eq!(fixture.request().await?, expected);
    }
    fixture.stop().await
}

#[tokio::test]
async fn stdio_callback_responses_keep_server_ids_after_metadata_removal() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut client = BufReader::new(connect(&fixture.proxy).await?);
    send(
        client.get_mut(),
        json!({
            "jsonrpc":"2.0","id":"init","method":"initialize",
            "params":{"capabilities":{"sampling":{}}},"_mcp_pool_timeout_ms":600_000
        }),
    )
    .await?;
    let initialize = fixture.request().await?;
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":initialize.get("id"),"result":{"capabilities":{}}}),
    )
    .await?;
    read(&mut client).await?;
    for identifier in [json!("server-id"), json!(42)] {
        let callback = json!({
            "jsonrpc":"2.0","id":identifier,"method":"sampling/createMessage","params":{}
        });
        reply(&fixture, callback.clone()).await?;
        assert_eq!(read(&mut client).await?, callback);
        send(
            client.get_mut(),
            json!({
                "jsonrpc":"2.0","id":identifier,"result":{"accepted":true},
                "_mcp_pool_timeout_ms":600_000
            }),
        )
        .await?;
        assert_eq!(
            fixture.request().await?,
            json!({"jsonrpc":"2.0","id":identifier,"result":{"accepted":true}})
        );
    }
    fixture.stop().await
}

#[tokio::test]
async fn stdio_metadata_requests_keep_cached_and_coalesced_initialize_behavior() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut leader = BufReader::new(connect(&fixture.proxy).await?);
    let mut follower = BufReader::new(connect(&fixture.proxy).await?);
    send(
        leader.get_mut(),
        json!({
            "jsonrpc":"2.0","id":"leader","method":"initialize",
            "params":{},"_mcp_pool_timeout_ms":600_000
        }),
    )
    .await?;
    let initialize = fixture.request().await?;
    assert!(initialize.get("_mcp_pool_timeout_ms").is_none());
    assert_ne!(initialize.get("id"), Some(&json!("leader")));
    send(
        follower.get_mut(),
        json!({
            "jsonrpc":"2.0","id":"follower","method":"initialize",
            "params":{},"_mcp_pool_timeout_ms":700_000
        }),
    )
    .await?;
    send(
        follower.get_mut(),
        json!({"jsonrpc":"2.0","id":"barrier","method":"ping"}),
    )
    .await?;
    let barrier = fixture.request().await?;
    assert_eq!(barrier.get("method"), Some(&json!("ping")));
    assert!(fixture.requests.try_recv().is_err());
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":barrier.get("id"),"result":{}}),
    )
    .await?;
    assert_eq!(
        read(&mut follower).await?.get("id"),
        Some(&json!("barrier"))
    );
    let result = json!({"capabilities":{}});
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":initialize.get("id"),"result":result}),
    )
    .await?;
    assert_eq!(
        read(&mut leader).await?,
        json!({"jsonrpc":"2.0","id":"leader","result":result})
    );
    assert_eq!(
        read(&mut follower).await?,
        json!({"jsonrpc":"2.0","id":"follower","result":result})
    );
    send(
        follower.get_mut(),
        json!({
            "jsonrpc":"2.0","id":"cached","method":"initialize",
            "params":{},"_mcp_pool_timeout_ms":600_000
        }),
    )
    .await?;
    assert_eq!(
        read(&mut follower).await?,
        json!({"jsonrpc":"2.0","id":"cached","result":result})
    );
    assert!(fixture.requests.try_recv().is_err());
    for client in [&mut leader, &mut follower] {
        send(
            client.get_mut(),
            json!({
                "jsonrpc":"2.0","method":"notifications/initialized",
                "_mcp_pool_timeout_ms":600_000
            }),
        )
        .await?;
    }
    assert_eq!(
        fixture.request().await?,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    );
    send(
        follower.get_mut(),
        json!({"jsonrpc":"2.0","id":"after-initialized","method":"ping"}),
    )
    .await?;
    assert_eq!(fixture.request().await?.get("method"), Some(&json!("ping")));
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}

#[tokio::test]
async fn stdio_invalid_metadata_is_rejected_without_dispatch_or_id_remapping() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut client = BufReader::new(connect(&fixture.proxy).await?);
    for invalid in [
        json!(0),
        json!(-1),
        json!(1.5),
        json!("600"),
        json!(true),
        Value::Null,
    ] {
        send(
            client.get_mut(),
            json!({
                "jsonrpc":"2.0","id":"invalid","method":"tools/call",
                "params":{"name":"inspect"},"_mcp_pool_timeout_ms":invalid
            }),
        )
        .await?;
        let response = read(&mut client).await?;
        assert_eq!(response.get("id"), Some(&json!("invalid")));
        assert_eq!(response.pointer("/error/code"), Some(&json!(-32602)));
        assert!(response.get("result").is_none());
        assert!(fixture.requests.try_recv().is_err());
    }
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"valid","method":"ping","_mcp_pool_timeout_ms":600_000}),
    )
    .await?;
    let valid = fixture.request().await?;
    assert_eq!(valid.get("method"), Some(&json!("ping")));
    assert!(valid.get("_mcp_pool_timeout_ms").is_none());
    reply(
        &fixture,
        json!({"jsonrpc":"2.0","id":valid.get("id"),"result":{}}),
    )
    .await?;
    assert_eq!(
        read(&mut client).await?,
        json!({"jsonrpc":"2.0","id":"valid","result":{}})
    );
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}
