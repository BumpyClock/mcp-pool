use anyhow::{Context, Result};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

use super::{McpClient, McpRpcError};
use std::time::Duration;

pub(super) type Server = BufReader<DuplexStream>;

pub(super) async fn read(server: &mut Server) -> Result<Value> {
    let mut frame = String::new();
    let count = server.read_line(&mut frame).await?;
    anyhow::ensure!(count > 0, "fixture reached EOF");
    Ok(serde_json::from_str(&frame)?)
}

pub(super) async fn send(server: &mut Server, message: Value) -> Result<()> {
    let frame = format!("{message}\n");
    server.get_mut().write_all(frame.as_bytes()).await?;
    Ok(())
}

pub(super) async fn reply(server: &mut Server, request: &Value, result: Value) -> Result<()> {
    let identifier = request.get("id").context("fixture request omitted ID")?;
    send(
        server,
        json!({"jsonrpc":"2.0","id":identifier,"result":result}),
    )
    .await
}

async fn initialize_server(server: &mut Server) -> Result<()> {
    let request = read(server).await?;
    assert_eq!(request.get("method"), Some(&json!("initialize")));
    assert_eq!(request.get("id"), Some(&json!(1)));
    assert!(
        request
            .get(crate::request_deadline::TIMEOUT_FIELD)
            .and_then(Value::as_u64)
            .is_some_and(|milliseconds| milliseconds > 0)
    );
    assert_eq!(request.pointer("/params/capabilities"), Some(&json!({})));
    assert_eq!(
        request.pointer("/params/protocolVersion"),
        Some(&json!("2025-06-18"))
    );
    send(
        server,
        json!({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"ready"}}),
    )
    .await?;
    reply(
        server,
        &request,
        json!({"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}}),
    )
    .await?;
    assert_eq!(
        read(server).await?,
        json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}})
    );
    Ok(())
}

pub(super) async fn fixture(deadline: Duration) -> Result<(McpClient, Server)> {
    let (stream, server) = tokio::io::duplex(4096);
    let mut server = BufReader::new(server);
    let (client, initialized) = tokio::join!(
        McpClient::initialize(Box::new(stream), deadline),
        initialize_server(&mut server)
    );
    initialized?;
    Ok((client?, server))
}

#[tokio::test]
async fn initialize_consumes_notifications_and_sends_initialized() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    let (notified, received) = tokio::join!(
        client.notify("notifications/message", json!({"data":"client"})),
        read(&mut server)
    );
    notified?;
    assert_eq!(
        received?,
        json!({"jsonrpc":"2.0","method":"notifications/message","params":{"data":"client"}})
    );
    Ok(())
}

#[tokio::test]
async fn callbacks_with_colliding_id_are_not_request_results() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    let server_work = async {
        let request = read(&mut server).await?;
        send(
            &mut server,
            json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed","params":{}}),
        )
        .await?;
        send(
            &mut server,
            json!({"jsonrpc":"2.0","id":2,"method":"sampling/createMessage","params":{}}),
        )
        .await?;
        let response = read(&mut server).await?;
        assert_eq!(response.get("id"), Some(&json!(2)));
        assert_eq!(response.pointer("/error/code"), Some(&json!(-32601)));
        send(
            &mut server,
            json!({"jsonrpc":"2.0","id":"ping-server","method":"ping"}),
        )
        .await?;
        assert_eq!(
            read(&mut server).await?,
            json!({"jsonrpc":"2.0","id":"ping-server","result":{}})
        );
        reply(&mut server, &request, json!({"answer":42})).await
    };
    let (result, served) = tokio::join!(client.request("tools/call", json!({})), server_work);
    served?;
    assert_eq!(result?, json!({"answer":42}));
    Ok(())
}

#[tokio::test]
async fn independent_connections_can_use_the_same_ids() -> Result<()> {
    let (mut first, mut first_server) = fixture(Duration::from_secs(1)).await?;
    let (mut second, mut second_server) = fixture(Duration::from_secs(1)).await?;
    let servers = async {
        let (first_request, second_request) =
            tokio::join!(read(&mut first_server), read(&mut second_server));
        let first_request = first_request?;
        let second_request = second_request?;
        assert_eq!(first_request.get("id"), Some(&json!(2)));
        assert_eq!(second_request.get("id"), Some(&json!(2)));
        reply(&mut second_server, &second_request, json!("second")).await?;
        reply(&mut first_server, &first_request, json!("first")).await?;
        Ok::<(), anyhow::Error>(())
    };
    let (first_result, second_result, served) = tokio::join!(
        first.request("tools/call", json!({})),
        second.request("tools/call", json!({})),
        servers
    );
    served?;
    assert_eq!(first_result?, json!("first"));
    assert_eq!(second_result?, json!("second"));
    Ok(())
}

#[tokio::test]
async fn rpc_errors_preserve_details_and_leave_connection_usable() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    let server_work = async {
        let request = read(&mut server).await?;
        send(
            &mut server,
            json!({"jsonrpc":"2.0","id":request.get("id"),"error":{"code":-32602,"message":"bad arguments","data":{"field":"path"}}}),
        )
        .await?;
        let next = read(&mut server).await?;
        assert_eq!(next.get("id"), Some(&json!(3)));
        reply(&mut server, &next, Value::Null).await
    };
    let client_work = async {
        let error = client
            .request("tools/call", json!({}))
            .await
            .err()
            .context("expected JSON-RPC error")?;
        let error = error.downcast_ref::<McpRpcError>().context("error type")?;
        assert_eq!(error.code, -32602);
        assert_eq!(error.message, "bad arguments");
        assert_eq!(error.data, Some(json!({"field":"path"})));
        assert!(!client.is_closed());
        assert_eq!(client.request("ping", json!({})).await?, Value::Null);
        Ok::<(), anyhow::Error>(())
    };
    let (result, served) = tokio::join!(client_work, server_work);
    served?;
    result
}

#[tokio::test]
async fn delayed_reply_retires_socket_without_unsafe_cancellation() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    assert!(!client.is_closed());
    client.deadline = Duration::from_millis(20);
    let server_work = async {
        let request = read(&mut server).await?;
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(reply(&mut server, &request, json!({})).await.is_err());
        let mut extra = String::new();
        assert_eq!(server.read_line(&mut extra).await?, 0);
        Ok::<(), anyhow::Error>(())
    };
    let (result, served) = tokio::join!(client.request("tools/call", json!({})), server_work);
    served?;
    let error = result.err().context("expected timeout")?;
    assert!(error.to_string().contains("deadline exceeded"));
    assert!(client.is_closed());
    let error = client
        .request("ping", json!({}))
        .await
        .err()
        .context("closed")?;
    assert!(error.to_string().contains("connection is closed"));
    Ok(())
}

#[tokio::test]
async fn dropped_request_future_retires_socket() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    let client_work = async {
        tokio::select! {
            result = client.request("tools/call", json!({})) => {
                bail_unexpected(result)?;
            }
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        assert!(client.request("ping", json!({})).await.is_err());
        Ok::<(), anyhow::Error>(())
    };
    let server_work = async {
        read(&mut server).await?;
        let mut extra = String::new();
        assert_eq!(server.read_line(&mut extra).await?, 0);
        Ok::<(), anyhow::Error>(())
    };
    let (result, served) = tokio::join!(client_work, server_work);
    served?;
    result
}

fn bail_unexpected(result: Result<Value>) -> Result<()> {
    anyhow::bail!("request unexpectedly completed: {result:?}")
}

#[tokio::test]
async fn tools_and_resources_follow_cursor_pages() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    let server_work = async {
        for (method, field) in [("tools/list", "tools"), ("resources/list", "resources")] {
            let first = read(&mut server).await?;
            assert_eq!(first.get("method"), Some(&json!(method)));
            assert_eq!(first.pointer("/params/cursor"), None);
            let mut result = json!({"nextCursor":""});
            result
                .as_object_mut()
                .context("fixture result object")?
                .insert(field.to_string(), json!([{"name":"first"}]));
            reply(&mut server, &first, result).await?;
            let next = read(&mut server).await?;
            assert_eq!(next.pointer("/params/cursor"), Some(&json!("")));
            let mut result = json!({});
            result
                .as_object_mut()
                .context("fixture result object")?
                .insert(field.to_string(), json!([{"name":"second"}]));
            reply(&mut server, &next, result).await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    let client_work = async {
        assert_eq!(
            client.list_tools().await?,
            vec![json!({"name":"first"}), json!({"name":"second"})]
        );
        assert_eq!(
            client.list_resources().await?,
            vec![json!({"name":"first"}), json!({"name":"second"})]
        );
        Ok::<(), anyhow::Error>(())
    };
    let (result, served) = tokio::join!(client_work, server_work);
    served?;
    result
}

#[tokio::test]
async fn pagination_rejects_cursor_cycles_and_invalid_cursors() -> Result<()> {
    for cursors in [vec![json!("a"), json!("b"), json!("a")], vec![Value::Null]] {
        let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
        let server_work = async {
            for cursor in cursors {
                let request = read(&mut server).await?;
                reply(
                    &mut server,
                    &request,
                    json!({"tools":[],"nextCursor":cursor}),
                )
                .await?;
            }
            Ok::<(), anyhow::Error>(())
        };
        let (result, served) = tokio::join!(client.list_tools(), server_work);
        served?;
        assert!(result.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn eof_and_malformed_frames_retire_connection() -> Result<()> {
    for frame in [
        "",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}",
        "not-json\n",
        "{\"jsonrpc\":\"2.0\",\"id\":\"2\",\"result\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{},\"error\":{}}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"error\":{\"message\":\"missing code\"}}\n",
    ] {
        let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
        let server_work = async move {
            read(&mut server).await?;
            server.get_mut().write_all(frame.as_bytes()).await?;
            server.get_mut().shutdown().await?;
            Ok::<(), anyhow::Error>(())
        };
        let (result, served) = tokio::join!(client.request("ping", json!({})), server_work);
        served?;
        assert!(result.is_err(), "frame was accepted: {frame}");
        assert!(client.reader.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn refuses_unsafe_cancellation_and_invalid_outgoing_params() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    assert!(
        client
            .notify("notifications/cancelled", json!({"requestId":2}))
            .await
            .is_err()
    );
    assert!(client.request("", json!({})).await.is_err());
    assert!(client.request("ping", Value::Null).await.is_err());
    let (result, served) = tokio::join!(client.request("ping", json!({})), async {
        let request = read(&mut server).await?;
        assert_eq!(request.get("id"), Some(&json!(2)));
        assert_eq!(request.get("method"), Some(&json!("ping")));
        reply(&mut server, &request, json!({})).await
    });
    served?;
    assert_eq!(result?, json!({}));
    Ok(())
}

#[tokio::test]
async fn rejects_unsupported_protocol_and_zero_deadline() -> Result<()> {
    let (stream, server) = tokio::io::duplex(4096);
    let mut server = BufReader::new(server);
    let (result, served) = tokio::join!(
        McpClient::initialize(Box::new(stream), Duration::from_secs(1)),
        async {
            let request = read(&mut server).await?;
            reply(
                &mut server,
                &request,
                json!({"protocolVersion":"unknown","capabilities":{}}),
            )
            .await
        }
    );
    served?;
    assert!(result.is_err());
    let (stream, _server) = tokio::io::duplex(64);
    assert!(
        McpClient::initialize(Box::new(stream), Duration::ZERO)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn pagination_deadline_does_not_reset_on_each_page() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    client.deadline = Duration::from_millis(100);
    let server_work = async {
        let first = read(&mut server).await?;
        tokio::time::sleep(Duration::from_millis(60)).await;
        reply(
            &mut server,
            &first,
            json!({"tools":[],"nextCursor":"second"}),
        )
        .await?;
        let second = read(&mut server).await?;
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            reply(&mut server, &second, json!({"tools":[]}))
                .await
                .is_err()
        );
        Ok::<(), anyhow::Error>(())
    };
    let (result, served) = tokio::join!(client.list_tools(), server_work);
    served?;
    let error = result.err().context("expected pagination timeout")?;
    assert!(error.to_string().contains("pagination deadline exceeded"));
    assert!(client.reader.is_none());
    Ok(())
}

#[tokio::test]
async fn stalled_notification_write_retires_connection() -> Result<()> {
    let (mut client, _server) = fixture(Duration::from_secs(1)).await?;
    client.deadline = Duration::from_millis(20);
    let error = client
        .notify("notifications/message", json!({"data":"x".repeat(8192)}))
        .await
        .err()
        .context("expected write timeout")?;
    assert!(error.to_string().contains("notification deadline exceeded"));
    assert!(client.reader.is_none());
    Ok(())
}

#[tokio::test]
async fn notifications_cannot_extend_request_deadline() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    client.deadline = Duration::from_millis(20);
    let server_work = async {
        read(&mut server).await?;
        loop {
            if send(
                &mut server,
                json!({"jsonrpc":"2.0","method":"notifications/progress","params":{}}),
            )
            .await
            .is_err()
            {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    let (result, served) = tokio::join!(client.request("ping", json!({})), server_work);
    served?;
    assert!(result.is_err());
    assert!(client.reader.is_none());
    Ok(())
}

#[tokio::test]
async fn split_utf8_frames_and_empty_lines_are_supported() -> Result<()> {
    let (mut client, mut server) = fixture(Duration::from_secs(1)).await?;
    let server_work = async {
        let request = read(&mut server).await?;
        let frame = format!(
            "\r\n{{\"jsonrpc\":\"2.0\",\"id\":{},\"result\":\"résumé\"}}\r\n",
            request.get("id").context("fixture request ID")?
        );
        for byte in frame.as_bytes() {
            server.get_mut().write_all(&[*byte]).await?;
            tokio::task::yield_now().await;
        }
        Ok::<(), anyhow::Error>(())
    };
    let (result, served) = tokio::join!(client.request("ping", json!({})), server_work);
    served?;
    assert_eq!(result?, json!("résumé"));
    Ok(())
}
