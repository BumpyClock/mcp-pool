use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::tests::*;
use super::*;

#[tokio::test]
async fn valid_response_precedes_terminal_sse_frame_error() -> TestResult {
    for legacy in [false, true] {
        let (listener, url) = fixture().await?;
        let server = tokio::spawn(async move {
            let (mut events, _) = listener.accept().await?;
            incoming(&mut events).await?;
            stream_headers(&mut events, "").await?;
            if legacy {
                events
                    .write_all(b"event: endpoint\ndata: /messages\n\n")
                    .await?;
                events.flush().await?;
                let (mut post, _) = listener.accept().await?;
                let request = incoming(&mut post).await?;
                assert_eq!(request.body.get("id"), Some(&Value::from("final-valid")));
                reply(&mut post, 202, "", "").await?;
            }
            events
                .write_all(
                    b"data: {\"jsonrpc\":\"2.0\",\"id\":\"final-valid\",\"result\":{\"ok\":true}}\r\n\r\ndata: \xff\r\n\r\n",
                )
                .await?;
            events.flush().await?;
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_secs(1), events.read(&mut byte)).await??,
                0,
                "terminal frame error must close the stream"
            );
            assert!(
                timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "requests must not replay"
            );
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        let (response_tx, mut responses) = mpsc::channel(16);
        let mut handle = spawn(url, legacy, response_tx).await?;
        handle
            .request_tx
            .send(r#"{"jsonrpc":"2.0","id":"final-valid","method":"tools/call"}"#.into())
            .await?;
        assert_eq!(
            message(&mut responses).await?,
            serde_json::json!({"jsonrpc":"2.0","id":"final-valid","result":{"ok":true}})
        );
        server.await??;
        if legacy {
            timeout(Duration::from_secs(1), handle.wait_for_exit()).await??;
        }
        stop(&mut handle).await?;
        assert!(
            responses.try_recv().is_err(),
            "no duplicate or replacement error"
        );
    }
    Ok(())
}

#[tokio::test]
async fn legacy_discovery_delivers_complete_message_before_terminal_frame_error() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        stream_headers(&mut stream, "").await?;
        stream
            .write_all(
                b"event: endpoint\ndata: /messages\n\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/test\"}\n\ndata: \xff\n\n",
            )
            .await?;
        stream.flush().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let result = spawn(url, true, response_tx).await;
    // A network chunk may end at the endpoint. Either discovery or receive owns the error.
    if let Ok(mut handle) = result {
        timeout(Duration::from_secs(1), handle.wait_for_exit()).await??;
        stop(&mut handle).await?;
    }
    assert_eq!(
        message(&mut responses).await?,
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/test"})
    );
    assert!(responses.try_recv().is_err());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn post_sse_remains_transparent_after_the_matching_response() -> TestResult {
    let (listener, url) = fixture().await?;
    let (release, released) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        stream_headers(&mut stream, "").await?;
        stream
            .write_all(b"data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{}}\n\n")
            .await?;
        stream.flush().await?;
        released.await?;
        stream
            .write_all(
                b"data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n",
            )
            .await?;
        stream.flush().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, false, response_tx).await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":5,"method":"tools/list"}"#.into())
        .await?;
    assert_eq!(
        message(&mut responses).await?.get("id"),
        Some(&Value::from(5))
    );
    release.send(()).map_err(|_| "fixture release closed")?;
    assert_eq!(
        message(&mut responses)
            .await?
            .get("method")
            .and_then(Value::as_str),
        Some("notifications/tools/list_changed")
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn initialize_sse_establishes_headers_before_stream_eof() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut initialization, _) = listener.accept().await?;
        incoming(&mut initialization).await?;
        stream_headers(&mut initialization, "Mcp-Session-Id: sse-session\r\n").await?;
        initialization.write_all(b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\"}}\r\n\r\n").await?;
        initialization.flush().await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(
            request.headers.get("mcp-session-id").map(String::as_str),
            Some("sse-session")
        );
        assert_eq!(
            request
                .headers
                .get("mcp-protocol-version")
                .map(String::as_str),
            Some("2025-03-26")
        );
        assert_eq!(
            request.body.get("method").and_then(Value::as_str),
            Some("notifications/initialized")
        );
        reply(&mut stream, 204, "", "").await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(
            request.body.get("method").and_then(Value::as_str),
            Some("tools/list")
        );
        reply(
            &mut stream,
            200,
            "Content-Type: application/json\r\n",
            r#"{"jsonrpc":"2.0","id":2,"result":{}}"#,
        )
        .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, false, response_tx).await?;
    handle.request_tx.send(initialize(1)).await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.into())
        .await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.into())
        .await?;
    assert_eq!(
        message(&mut responses).await?.get("id"),
        Some(&Value::from(1))
    );
    assert_eq!(
        message(&mut responses).await?.get("id"),
        Some(&Value::from(2))
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn read_deadline_returns_correlated_error_and_closes_stream() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        stream_headers(&mut stream, "").await?;
        stream.flush().await?;
        let mut byte = [0];
        assert_eq!(
            timeout(Duration::from_secs(3), stream.read(&mut byte)).await??,
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
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":"read-timeout","method":"tools/call"}"#.into())
        .await?;
    let response = message(&mut responses).await?;
    assert_eq!(
        response.get("id"),
        Some(&Value::String("read-timeout".into()))
    );
    assert!(
        response
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("read deadline"))
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn total_request_deadline_is_not_extended_by_sse_heartbeats() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        stream_headers(&mut stream, "").await?;
        let mut byte = [0];
        loop {
            tokio::select! {
                result = stream.read(&mut byte) => {
                    assert_eq!(result?, 0);
                    break;
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {
                    stream.write_all(b": heartbeat\n\n").await?;
                    stream.flush().await?;
                }
            }
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, false, response_tx).await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":20,"method":"tools/call"}"#.into())
        .await?;
    let response = message(&mut responses).await?;
    assert_eq!(response.get("id"), Some(&Value::from(20)));
    assert!(
        response
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("request deadline"))
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn oversized_json_response_is_bounded_and_correlated() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        let body = " ".repeat(sse_parser::FRAME_LIMIT + 1);
        let result = reply(
            &mut stream,
            200,
            "Content-Type: application/json\r\n",
            &body,
        )
        .await;
        if let Err(error) = result {
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::ConnectionAborted
                ),
                "size-bound disconnect returned an unexpected error"
            );
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, false, response_tx).await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":30,"method":"tools/call"}"#.into())
        .await?;
    let response = message(&mut responses).await?;
    assert_eq!(response.get("id"), Some(&Value::from(30)));
    assert!(
        response
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("size limit"))
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn legacy_sse_rejects_cross_origin_message_endpoint() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        stream_headers(&mut stream, "").await?;
        stream
            .write_all(b"event: endpoint\ndata: http://different.invalid/messages\n\n")
            .await?;
        stream.flush().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, _responses) = mpsc::channel(16);
    let result = spawn(url, true, response_tx).await;
    assert!(
        result
            .err()
            .is_some_and(|error| error.to_string().contains("same origin"))
    );
    server.await??;
    Ok(())
}

#[tokio::test]
async fn legacy_stream_failure_returns_error_for_pending_request_id() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut events, _) = listener.accept().await?;
        incoming(&mut events).await?;
        stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\ndata: /messages\n\n")
            .await?;
        events.flush().await?;
        let (mut post, _) = listener.accept().await?;
        incoming(&mut post).await?;
        reply(&mut post, 202, "", "").await?;
        events
            .write_all(b"event: message\ndata: invalid\n\n")
            .await?;
        events.flush().await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, true, response_tx).await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":"legacy-broken","method":"tools/call"}"#.into())
        .await?;
    let response = message(&mut responses).await?;
    assert_eq!(
        response.get("id"),
        Some(&Value::String("legacy-broken".into()))
    );
    assert!(response.get("error").is_some());
    timeout(Duration::from_secs(1), handle.wait_for_exit()).await??;
    handle.shutdown().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn legacy_expired_endpoint_rejects_later_posts_without_replay() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut events, _) = listener.accept().await?;
        incoming(&mut events).await?;
        stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\ndata: /messages?session=expired\n\n")
            .await?;
        events.flush().await?;
        let (mut post, _) = listener.accept().await?;
        incoming(&mut post).await?;
        reply(&mut post, 404, "", "").await?;
        assert!(
            timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err()
        );
        let mut byte = [0];
        assert_eq!(events.read(&mut byte).await?, 0);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, true, response_tx).await?;
    for identifier in [50, 51] {
        handle
            .request_tx
            .send(
                serde_json::json!({"jsonrpc":"2.0","id":identifier,"method":"tools/call"})
                    .to_string(),
            )
            .await?;
        let response = message(&mut responses).await?;
        assert_eq!(response.get("id"), Some(&Value::from(identifier)));
        assert!(
            response
                .pointer("/error/message")
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains("session expired"))
        );
    }
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}
