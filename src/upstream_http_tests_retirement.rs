use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use super::tests::*;
use super::*;

#[tokio::test]
async fn deliberate_shutdown_deletes_session_with_negotiated_headers() -> TestResult {
    for status in [204, 405] {
        let (listener, url) = fixture().await?;
        let server = tokio::spawn(async move {
            let (mut initialization, _) = listener.accept().await?;
            incoming(&mut initialization).await?;
            reply(
                &mut initialization,
                200,
                "Content-Type: application/json\r\nMcp-Session-Id: delete-session\r\n",
                r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05"}}"#,
            )
            .await?;
            let (mut deletion, _) = listener.accept().await?;
            let request = incoming(&mut deletion).await?;
            assert_eq!(request.method, "DELETE");
            assert_eq!(request.path, "/mcp");
            assert_eq!(
                request.headers.get("mcp-session-id").map(String::as_str),
                Some("delete-session")
            );
            assert_eq!(
                request
                    .headers
                    .get("mcp-protocol-version")
                    .map(String::as_str),
                Some("2024-11-05")
            );
            assert!(request.body.is_null());
            reply(&mut deletion, status, "", "").await?;
            assert!(
                timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        let (response_tx, mut responses) = mpsc::channel(16);
        let mut handle = spawn(url, false, response_tx).await?;
        handle.request_tx.send(initialize(1)).await?;
        message(&mut responses).await?;
        stop(&mut handle).await?;
        handle.wait_for_exit().await?;
        server.await??;
    }
    Ok(())
}

#[tokio::test]
async fn remote_delete_timeout_does_not_poison_local_retirement_or_replay() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut initialization, _) = listener.accept().await?;
        incoming(&mut initialization).await?;
        reply(
            &mut initialization,
            200,
            "Content-Type: application/json\r\nMcp-Session-Id: delete-timeout\r\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#,
        )
        .await?;
        let (mut deletion, _) = listener.accept().await?;
        assert_eq!(incoming(&mut deletion).await?.method, "DELETE");
        let mut byte = [0];
        assert_eq!(
            timeout(Duration::from_secs(2), deletion.read(&mut byte)).await??,
            0
        );
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, false, response_tx).await?;
    handle.request_tx.send(initialize(1)).await?;
    message(&mut responses).await?;
    stop(&mut handle).await?;
    handle.wait_for_exit().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn definitive_legacy_disconnect_allows_same_proxy_restart_without_replay() -> TestResult {
    let (listener, url) = fixture().await?;
    let port = listener.local_addr()?.port();
    let (disconnect, disconnection) = oneshot::channel();
    let (first_post, posted) = oneshot::channel();
    let (replay_checked, replay_check) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut events, _) = listener.accept().await?;
        incoming(&mut events).await?;
        stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\ndata: /messages?generation=first\n\n")
            .await?;
        events.flush().await?;
        let (mut post, _) = listener.accept().await?;
        let request = incoming(&mut post).await?;
        assert_eq!(
            request.body.get("method").and_then(Value::as_str),
            Some("tools/call")
        );
        reply(&mut post, 202, "", "").await?;
        first_post
            .send(())
            .map_err(|_| "fixture first POST waiter closed")?;
        disconnection.await?;
        drop(events);
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        replay_checked
            .send(())
            .map_err(|_| "fixture replay check waiter closed")?;
        let (mut events, _) = listener.accept().await?;
        let request = incoming(&mut events).await?;
        assert_eq!(request.method, "GET");
        stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\ndata: /messages?generation=second\n\n")
            .await?;
        events.flush().await?;
        let (mut post, _) = listener.accept().await?;
        let request = incoming(&mut post).await?;
        assert_eq!(request.path, "/messages?generation=second");
        assert_eq!(
            request.body.get("method").and_then(Value::as_str),
            Some("tools/list")
        );
        let identifier = request
            .body
            .get("id")
            .ok_or("missing rewritten request ID")?;
        reply(&mut post, 202, "", "").await?;
        let response = serde_json::json!({"jsonrpc":"2.0","id":identifier,"result":{"tools":[]}});
        events
            .write_all(format!("data: {response}\n\n").as_bytes())
            .await?;
        events.flush().await?;
        let mut byte = [0];
        assert_eq!(
            timeout(Duration::from_secs(2), events.read(&mut byte)).await??,
            0
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    #[cfg(windows)]
    let path = std::path::PathBuf::from(format!(
        r"\\.\pipe\mcp-pool-http-retirement-{}-{port}",
        std::process::id(),
    ));
    #[cfg(unix)]
    let path = std::env::temp_dir().join(format!("pool-http-{}-{port}.sock", std::process::id(),));
    let proxy = Arc::new(crate::socket_proxy::SocketProxy::new(
        format!("http-retirement-{port}"),
        path.clone(),
        crate::upstream::UpstreamSpec::Http {
            url,
            sse: true,
            headers: Default::default(),
            timeout_ms: None,
            auth: None,
        },
        true,
        None,
    ));
    proxy.start().await?;
    let mut first = crate::transport::connect(&path).await?;
    first
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"before\",\"method\":\"tools/call\"}\n")
        .await?;
    first.flush().await?;
    timeout(Duration::from_secs(2), posted).await??;
    disconnect
        .send(())
        .map_err(|_| "fixture disconnection waiter closed")?;
    let mut first = BufReader::new(first);
    let mut terminal_line = String::new();
    assert!(
        timeout(Duration::from_secs(2), first.read_line(&mut terminal_line)).await?? > 0,
        "the terminal transport error must reach the caller before EOF"
    );
    let terminal_response: Value = serde_json::from_str(&terminal_line)?;
    assert_eq!(
        terminal_response,
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": "before",
            "error": {
                "code": -32000,
                "message": "Legacy SSE connection failed; request was not replayed"
            }
        })
    );
    terminal_line.clear();
    assert_eq!(
        timeout(Duration::from_secs(2), first.read_line(&mut terminal_line)).await??,
        0,
        "the retired generation must close after its terminal error"
    );
    timeout(Duration::from_secs(2), async {
        while proxy.status() != crate::types::ServerStatus::Stopped {
            if proxy.status() == crate::types::ServerStatus::Failed {
                return Err(io::Error::other(
                    "definitive HTTP disconnect poisoned retirement",
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        Ok::<(), io::Error>(())
    })
    .await??;
    assert!(proxy.readiness().retirement_error.is_none());
    timeout(Duration::from_secs(2), replay_check).await??;
    assert!(proxy.restart().await?);
    let mut second = crate::transport::connect(&path).await?;
    second
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"after\",\"method\":\"tools/list\"}\n")
        .await?;
    second.flush().await?;
    let mut reader = BufReader::new(second);
    let mut line = String::new();
    assert!(timeout(Duration::from_secs(2), reader.read_line(&mut line)).await?? > 0);
    let response: Value = serde_json::from_str(&line)?;
    assert_eq!(response.get("id"), Some(&Value::String("after".into())));
    assert!(response.get("result").is_some());
    proxy.stop().await?;
    server.await??;
    Ok(())
}
