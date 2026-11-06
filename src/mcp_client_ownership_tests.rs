use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::McpRpcError;
use super::tests::{fixture, read, reply, send};

#[tokio::test]
async fn large_results_and_error_data_preserve_null_and_absent_data() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(5)).await?;
    let payload = json!({"text":"x".repeat(1024 * 1024),"nested":[{"value":null},[1,2,3]]});
    let server_payload = payload.clone();
    let server_work = async {
        let request = read(&mut server).await?;
        assert_eq!(request.pointer("/params/arguments"), Some(&server_payload));
        reply(&mut server, &request, server_payload.clone()).await?;
        for data in [Some(server_payload), Some(Value::Null), None] {
            let request = read(&mut server).await?;
            let mut error = json!({"code":-32602,"message":"bad arguments"});
            if let Some(data) = data {
                error
                    .as_object_mut()
                    .context("fixture error")?
                    .insert("data".to_owned(), data);
            }
            send(
                &mut server,
                json!({"jsonrpc":"2.0","id":request.get("id"),"error":error}),
            )
            .await?;
        }
        let request = read(&mut server).await?;
        reply(&mut server, &request, Value::Null).await
    };
    let client_work = async {
        assert_eq!(
            client
                .request("tools/call", json!({"arguments":payload}))
                .await?,
            payload
        );
        for expected in [Some(&payload), Some(&Value::Null), None] {
            let error = client
                .request("ping", json!({}))
                .await
                .err()
                .context("expected error")?;
            let error = error
                .downcast_ref::<McpRpcError>()
                .context("RPC error type")?;
            assert_eq!(error.code, -32602);
            assert_eq!(error.message, "bad arguments");
            assert_eq!(error.data.as_ref(), expected);
            assert!(!client.is_closed());
        }
        assert_eq!(client.request("ping", json!({})).await?, Value::Null);
        Ok::<_, anyhow::Error>(())
    };
    let (result, served) = tokio::join!(client_work, server_work);
    served?;
    result
}

#[tokio::test]
async fn malformed_responses_are_rejected_and_retire_the_socket() -> Result<()> {
    for response in [
        json!({"jsonrpc":"2.0","id":3,"result":{},"error":{}}),
        json!({"jsonrpc":"2.0","id":2,"result":null,"error":null}),
        json!({"jsonrpc":"2.0","id":2}),
        json!({"jsonrpc":"2.0","id":2,"error":[]}),
        json!({"jsonrpc":"2.0","id":2,"error":{"code":1.5,"message":null}}),
        json!({"jsonrpc":"2.0","id":2,"error":{"code":-1,"message":null}}),
        json!({"jsonrpc":"2.0","id":2,"error":{"code":-1}}),
    ] {
        let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
        let server_work = async {
            read(&mut server).await?;
            send(&mut server, response).await
        };
        let (result, served) = tokio::join!(client.request("ping", json!({})), server_work);
        served?;
        let error = result.err().context("expected protocol error")?;
        assert!(error.downcast_ref::<McpRpcError>().is_none());
        assert!(client.is_closed());
    }
    Ok(())
}

#[tokio::test]
async fn paginated_large_entries_preserve_order_for_tools_and_resources() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(5)).await?;
    let entries = vec![
        json!({"name":"first","metadata":{"text":"x".repeat(256 * 1024),"values":[null,1,true]}}),
        json!({"name":"second","metadata":{"text":"y".repeat(256 * 1024)}}),
        json!({"name":"third","metadata":{"text":"z".repeat(256 * 1024)}}),
    ];
    let server_entries = entries.clone();
    let server_work = async {
        for field in ["tools", "resources"] {
            for (index, entry) in server_entries.iter().enumerate() {
                let request = read(&mut server).await?;
                let expected_cursor = match index {
                    0 => None,
                    1 => Some(json!("")),
                    _ => Some(json!("last")),
                };
                assert_eq!(request.pointer("/params/cursor"), expected_cursor.as_ref());
                let mut result = serde_json::Map::from_iter([(field.to_owned(), json!([entry]))]);
                match index {
                    0 => {
                        result.insert("nextCursor".to_owned(), json!(""));
                    }
                    1 => {
                        result.insert("nextCursor".to_owned(), json!("last"));
                    }
                    _ => {}
                }
                reply(&mut server, &request, Value::Object(result)).await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    let client_work = async {
        assert_eq!(client.list_tools().await?, entries);
        assert_eq!(client.list_resources().await?, entries);
        Ok::<_, anyhow::Error>(())
    };
    let (result, served) = tokio::join!(client_work, server_work);
    served?;
    result
}

#[tokio::test]
async fn invalid_pagination_is_rejected_and_keeps_connection_usable() -> Result<()> {
    for page in [
        json!({"tools":null,"nextCursor":false}),
        json!({"tools":[{},"bad"],"nextCursor":false}),
        json!({"tools":[{}],"nextCursor":false}),
    ] {
        let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
        let server_work = async {
            let request = read(&mut server).await?;
            reply(&mut server, &request, page).await?;
            let request = read(&mut server).await?;
            reply(&mut server, &request, json!({})).await
        };
        let client_work = async {
            assert!(client.list_tools().await.is_err());
            assert!(!client.is_closed());
            assert_eq!(client.request("ping", json!({})).await?, json!({}));
            Ok::<_, anyhow::Error>(())
        };
        let (result, served) = tokio::join!(client_work, server_work);
        served?;
        result?;
    }
    Ok(())
}
