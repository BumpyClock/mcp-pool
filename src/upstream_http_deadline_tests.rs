use super::tests::{
    TestResult, fixture, incoming, initialize, message, reply, stop, stream_headers,
};
use super::*;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn packet(identifier: i64, method: &str, timeout_ms: Value) -> String {
    serde_json::json!({
        "jsonrpc":"2.0", "id":identifier, "method":method,
        "_mcp_pool_timeout_ms":timeout_ms,
    })
    .to_string()
}

fn assert_private_field_removed(request: &super::tests::Incoming) {
    assert!(request.body.get("_mcp_pool_timeout_ms").is_none());
}

#[tokio::test]
async fn caller_deadline_extends_existing_http_session_without_reinitialization() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(
            request.body.get("method").and_then(Value::as_str),
            Some("initialize")
        );
        assert_private_field_removed(&request);
        reply(
            &mut stream,
            200,
            "Content-Type: application/json\r\nMcp-Session-Id: one-existing-session\r\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#,
        )
        .await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(
            request.body.get("method").and_then(Value::as_str),
            Some("tools/call")
        );
        assert_eq!(
            request.headers.get("mcp-session-id").map(String::as_str),
            Some("one-existing-session")
        );
        assert_private_field_removed(&request);
        let body = r#"{"jsonrpc":"2.0","id":2,"result":{"reused":true}}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes()).await?;
        tokio::time::sleep(Duration::from_millis(220)).await;
        stream.write_all(body.as_bytes()).await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(request.method, "DELETE");
        assert_eq!(
            request.headers.get("mcp-session-id").map(String::as_str),
            Some("one-existing-session")
        );
        reply(&mut stream, 200, "", "").await?;
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle =
        spawn_configured(url, false, BTreeMap::new(), Some(100), None, responses).await?;
    handle.request_tx.send(initialize(1)).await?;
    assert!(message(&mut receiver).await?.get("result").is_some());
    handle
        .request_tx
        .send(packet(2, "tools/call", Value::from(600)).into())
        .await?;
    assert_eq!(
        message(&mut receiver).await?.pointer("/result/reused"),
        Some(&Value::Bool(true))
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn caller_deadline_extends_legacy_response_wait_on_the_existing_stream() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut events, _) = listener.accept().await?;
        assert_eq!(incoming(&mut events).await?.method, "GET");
        stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\ndata: /messages\n\n")
            .await?;
        for (identifier, method) in [(1, "initialize"), (2, "tools/call")] {
            let (mut stream, _) = listener.accept().await?;
            let request = incoming(&mut stream).await?;
            assert_eq!(request.method, "POST");
            assert_eq!(
                request.body.get("method").and_then(Value::as_str),
                Some(method)
            );
            assert_private_field_removed(&request);
            reply(&mut stream, 202, "", "").await?;
            tokio::time::sleep(Duration::from_millis(220)).await;
            let result = if identifier == 1 {
                serde_json::json!({"protocolVersion":"2025-03-26"})
            } else {
                serde_json::json!({"reused":true})
            };
            let response = serde_json::json!({"jsonrpc":"2.0","id":identifier,"result":result});
            events
                .write_all(format!("event: message\ndata: {response}\n\n").as_bytes())
                .await?;
        }
        let mut buffer = [0u8; 1];
        assert_eq!(events.read(&mut buffer).await?, 0);
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle =
        spawn_configured(url, true, BTreeMap::new(), Some(100), None, responses).await?;
    handle
        .request_tx
        .send(packet(1, "initialize", Value::from(600)).into())
        .await?;
    assert!(message(&mut receiver).await?.get("result").is_some());
    handle
        .request_tx
        .send(packet(2, "tools/call", Value::from(600)).into())
        .await?;
    assert_eq!(
        message(&mut receiver).await?.pointer("/result/reused"),
        Some(&Value::Bool(true))
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn shorter_caller_deadline_does_not_cancel_shared_initialize_or_discovery() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        for identifier in [1, 2, 3] {
            let (mut stream, _) = listener.accept().await?;
            let request = incoming(&mut stream).await?;
            assert_private_field_removed(&request);
            tokio::time::sleep(Duration::from_millis(50)).await;
            if identifier != 3 {
                let result = if identifier == 1 {
                    serde_json::json!({"protocolVersion":"2025-03-26"})
                } else {
                    serde_json::json!({"tools":[]})
                };
                reply(
                    &mut stream,
                    200,
                    "Content-Type: application/json\r\n",
                    &serde_json::json!({"jsonrpc":"2.0","id":identifier,"result":result})
                        .to_string(),
                )
                .await?;
            }
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle =
        spawn_configured(url, false, BTreeMap::new(), Some(250), None, responses).await?;
    for (identifier, method) in [(1, "initialize"), (2, "tools/list")] {
        handle
            .request_tx
            .send(packet(identifier, method, Value::from(5)).into())
            .await?;
        assert!(message(&mut receiver).await?.get("result").is_some());
    }
    let paginated = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"cursor":"second"},"_mcp_pool_timeout_ms":5});
    handle.request_tx.send(paginated.to_string().into()).await?;
    assert_eq!(
        message(&mut receiver)
            .await?
            .pointer("/error/message")
            .and_then(Value::as_str),
        Some("HTTP request deadline exceeded; request was not replayed")
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn invalid_private_deadlines_are_correlated_and_never_sent_upstream() -> TestResult {
    let (listener, url) = fixture().await?;
    let (responses, mut receiver) = mpsc::channel(8);
    let mut handle = spawn_configured(url, false, BTreeMap::new(), None, None, responses).await?;
    for invalid in [
        Value::Null,
        Value::from(0),
        Value::from(-1),
        Value::from(1.5),
        Value::from("synthetic-private-secret"),
    ] {
        handle
            .request_tx
            .send(packet(7, "tools/call", invalid).into())
            .await?;
        let response = message(&mut receiver).await?;
        assert_eq!(response.get("id"), Some(&Value::from(7)));
        assert_eq!(
            response.pointer("/error/message").and_then(Value::as_str),
            Some("Pool request timeout must be positive u64 milliseconds")
        );
        assert!(!response.to_string().contains("synthetic-private-secret"));
    }
    assert!(
        timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    stop(&mut handle).await?;
    Ok(())
}

#[tokio::test]
async fn fallback_keeps_caller_deadline_private_through_discovery_and_post() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        assert_private_field_removed(&incoming(&mut stream).await?);
        reply(&mut stream, 405, "", "").await?;
        let (mut events, _) = listener.accept().await?;
        assert_eq!(incoming(&mut events).await?.method, "GET");
        tokio::time::sleep(Duration::from_millis(220)).await;
        stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\ndata: /messages\n\n")
            .await?;
        let (mut stream, _) = listener.accept().await?;
        assert_private_field_removed(&incoming(&mut stream).await?);
        reply(&mut stream, 202, "", "").await?;
        tokio::time::sleep(Duration::from_millis(220)).await;
        events.write_all(b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\"}}\n\n").await?;
        let mut buffer = [0u8; 1];
        assert_eq!(events.read(&mut buffer).await?, 0);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle =
        spawn_configured(url, false, BTreeMap::new(), Some(100), None, responses).await?;
    handle
        .request_tx
        .send(packet(1, "initialize", Value::from(600)).into())
        .await?;
    assert!(message(&mut receiver).await?.get("result").is_some());
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

async fn socket_message(
    stream: &mut BufReader<crate::transport::LocalStream>,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let mut line = String::new();
    let length = timeout(Duration::from_secs(2), stream.read_line(&mut line)).await??;
    if length == 0 {
        return Err("deadline fixture socket closed".into());
    }
    Ok(serde_json::from_str(&line)?)
}

#[tokio::test]
async fn two_socket_clients_reuse_one_pool_with_different_request_deadlines() -> TestResult {
    let (listener, url) = fixture().await?;
    let port = listener.local_addr()?.port();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(
            request.body.get("method").and_then(Value::as_str),
            Some("initialize")
        );
        assert_private_field_removed(&request);
        let identifier = request
            .body
            .get("id")
            .ok_or("fixture initialize ID missing")?;
        reply(&mut stream, 200, "Content-Type: application/json\r\nMcp-Session-Id: one-pooled-session\r\n",
            &serde_json::json!({"jsonrpc":"2.0","id":identifier,"result":{"protocolVersion":"2025-03-26"}}).to_string()).await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(
            request.body.get("method").and_then(Value::as_str),
            Some("tools/call"),
            "second client initialize must reuse the cache"
        );
        assert_private_field_removed(&request);
        assert_eq!(
            request.headers.get("mcp-session-id").map(String::as_str),
            Some("one-pooled-session")
        );
        tokio::time::sleep(Duration::from_millis(220)).await;
        let identifier = request.body.get("id").ok_or("fixture tool ID missing")?;
        reply(
            &mut stream,
            200,
            "Content-Type: application/json\r\n",
            &serde_json::json!({"jsonrpc":"2.0","id":identifier,"result":{"onePool":true}})
                .to_string(),
        )
        .await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(request.method, "DELETE");
        assert_eq!(
            request.headers.get("mcp-session-id").map(String::as_str),
            Some("one-pooled-session")
        );
        reply(&mut stream, 200, "", "").await?;
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    #[cfg(windows)]
    let path = std::path::PathBuf::from(format!(
        r"\\.\pipe\mcp-pool-deadline-{}-{port}",
        std::process::id()
    ));
    #[cfg(unix)]
    let path =
        std::env::current_dir()?.join(format!(".deadline-pool-{}-{port}.sock", std::process::id()));
    let proxy = Arc::new(crate::socket_proxy::SocketProxy::new(
        format!("deadline-{port}"),
        path.clone(),
        crate::upstream::UpstreamSpec::Http {
            url,
            sse: false,
            headers: BTreeMap::new(),
            timeout_ms: Some(100),
            auth: None,
        },
        true,
        None,
    ));
    proxy.start().await?;
    let mut first = BufReader::new(crate::transport::connect(&path).await?);
    first
        .get_mut()
        .write_all(format!("{}\n", packet(1, "initialize", Value::from(100))).as_bytes())
        .await?;
    assert_eq!(
        socket_message(&mut first).await?.get("id"),
        Some(&Value::from(1))
    );
    let mut second = BufReader::new(crate::transport::connect(&path).await?);
    second
        .get_mut()
        .write_all(format!("{}\n", packet(1, "initialize", Value::from(600))).as_bytes())
        .await?;
    assert_eq!(
        socket_message(&mut second).await?.get("id"),
        Some(&Value::from(1))
    );
    second
        .get_mut()
        .write_all(format!("{}\n", packet(2, "tools/call", Value::from(600))).as_bytes())
        .await?;
    let response = socket_message(&mut second).await?;
    assert_eq!(response.get("id"), Some(&Value::from(2)));
    assert_eq!(
        response.pointer("/result/onePool"),
        Some(&Value::Bool(true))
    );
    proxy.stop().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn legacy_idle_stream_survives_request_expiration_and_shutdown() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut events, _) = listener.accept().await?;
        assert_eq!(incoming(&mut events).await?.method, "GET");
        stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\ndata: /messages\n\n")
            .await?;
        for identifier in [1, 2, 3] {
            let (mut stream, _) = listener.accept().await?;
            let request = incoming(&mut stream).await?;
            assert_eq!(
                request.method, "POST",
                "idle or expired requests must not reopen the GET"
            );
            assert_eq!(request.body.get("id"), Some(&Value::from(identifier)));
            if identifier != 1 {
                assert_eq!(
                    request
                        .headers
                        .get("mcp-protocol-version")
                        .map(String::as_str),
                    Some("2025-03-26")
                );
            }
            reply(&mut stream, 202, "", "").await?;
            if identifier == 2 {
                continue;
            }
            let result = if identifier == 1 {
                serde_json::json!({"protocolVersion":"2025-03-26"})
            } else {
                serde_json::json!({"sameStream":true})
            };
            let response = serde_json::json!({"jsonrpc":"2.0","id":identifier,"result":result});
            events
                .write_all(format!("event: message\ndata: {response}\n\n").as_bytes())
                .await?;
        }
        let mut buffer = [0u8; 1];
        assert_eq!(
            timeout(Duration::from_millis(250), events.read(&mut buffer)).await??,
            0
        );
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle =
        spawn_configured(url, true, BTreeMap::new(), Some(50), None, responses).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle.request_tx.send(initialize(1)).await?;
    assert_eq!(
        message(&mut receiver)
            .await?
            .pointer("/result/protocolVersion")
            .and_then(Value::as_str),
        Some("2025-03-26")
    );
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call"}"#.into())
        .await?;
    assert_eq!(
        timeout(Duration::from_millis(200), message(&mut receiver))
            .await??
            .pointer("/error/message")
            .and_then(Value::as_str),
        Some("Legacy SSE request deadline exceeded; request was not replayed")
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call"}"#.into())
        .await?;
    assert_eq!(
        message(&mut receiver).await?.pointer("/result/sameStream"),
        Some(&Value::Bool(true))
    );
    timeout(Duration::from_millis(250), handle.shutdown()).await??;
    server.await??;
    Ok(())
}
