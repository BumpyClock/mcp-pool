use super::tests::{TestResult, fixture, incoming, message, reply, stop, stream_headers};
use super::*;
use tokio::io::AsyncWriteExt;

fn headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "Authorization".into(),
            "Bearer synthetic-static-secret".into(),
        ),
        ("X-Api-Key".into(), "synthetic-api-secret".into()),
    ])
}

fn assert_headers(request: &super::tests::Incoming) {
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("Bearer synthetic-static-secret")
    );
    assert_eq!(
        request.headers.get("x-api-key").map(String::as_str),
        Some("synthetic-api-secret")
    );
}

#[tokio::test]
async fn configured_headers_reach_streamable_post() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_headers(&request);
        assert!(!request.headers.contains_key("mcp-session-id"));
        assert!(!request.headers.contains_key("mcp-protocol-version"));
        reply(
            &mut stream,
            200,
            "Content-Type: application/json\r\n",
            r#"{"jsonrpc":"2.0","id":7,"result":{}}"#,
        )
        .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut configured = headers();
    configured.insert("McP-SeSsIoN-Id".into(), "configured-session".into());
    configured.insert("MCP-Protocol-Version".into(), "configured-version".into());
    let mut handle = spawn_configured(url, false, configured, None, None, responses).await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#.into())
        .await?;
    assert_eq!(
        message(&mut receiver).await?.get("id"),
        Some(&Value::from(7))
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn configured_headers_reach_legacy_get_and_post() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut events, _) = listener.accept().await?;
        let request = incoming(&mut events).await?;
        assert_eq!(request.method, "GET");
        assert_headers(&request);
        stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\ndata: /messages\n\n")
            .await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(request.path, "/messages");
        assert_headers(&request);
        reply(&mut stream, 202, "", "").await?;
        events
            .write_all(b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n")
            .await?;
        let mut buffer = [0u8; 1];
        use tokio::io::AsyncReadExt;
        assert_eq!(events.read(&mut buffer).await?, 0);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle = spawn_configured(url, true, headers(), None, None, responses).await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#.into())
        .await?;
    assert_eq!(
        message(&mut receiver).await?.get("id"),
        Some(&Value::from(7))
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn configured_deadline_is_effective_and_errors_do_not_echo_secrets() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        assert_headers(&incoming(&mut stream).await?);
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle = spawn_configured(url, false, headers(), Some(50), None, responses).await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#.into())
        .await?;
    let response = timeout(Duration::from_millis(150), message(&mut receiver)).await??;
    assert_eq!(
        response.pointer("/error/message").and_then(Value::as_str),
        Some("HTTP request deadline exceeded; request was not replayed")
    );
    assert!(!response.to_string().contains("synthetic"));
    stop(&mut handle).await?;
    server.await??;
    let options = Options::new(BTreeMap::new(), Some(120_000), None)?;
    assert_eq!(options.request_timeout, Duration::from_secs(120));
    assert_eq!(options.read_timeout, Duration::from_secs(120));
    Ok(())
}

#[tokio::test]
async fn configured_deadline_can_exceed_default_request_and_read_caps() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        let body = r#"{"jsonrpc":"2.0","id":7,"result":{}}"#;
        stream.write_all(format!("HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n", body.len()).as_bytes()).await?;
        tokio::time::sleep(Duration::from_millis(2200)).await;
        stream.write_all(body.as_bytes()).await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle =
        spawn_configured(url, false, BTreeMap::new(), Some(3500), None, responses).await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":7,"method":"tools/list"}"#.into())
        .await?;
    let response = message(&mut receiver).await?;
    assert!(response.get("result").is_some(), "{response}");
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn rejected_header_and_url_values_are_redacted() -> TestResult {
    let (responses, _receiver) = mpsc::channel(4);
    let invalid = BTreeMap::from([("X-Secret".into(), "synthetic-secret\r\ninjected".into())]);
    let error = spawn_configured(
        "http://127.0.0.1:1".into(),
        false,
        invalid,
        None,
        None,
        responses.clone(),
    )
    .await
    .err()
    .ok_or("invalid header accepted")?;
    assert_eq!(error.to_string(), "Invalid configured HTTP headers");
    let error = spawn_configured(
        "http://synthetic-secret@127.0.0.1:1".into(),
        false,
        BTreeMap::new(),
        None,
        None,
        responses,
    )
    .await
    .err()
    .ok_or("credential URL accepted")?;
    assert_eq!(
        error.to_string(),
        "HTTP upstream URL must not contain credentials"
    );
    Ok(())
}

#[tokio::test]
async fn initial_404_or_405_can_switch_to_legacy_sse_and_retire() -> TestResult {
    for rejection in [404, 405] {
        let (listener, url) = fixture().await?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            assert_headers(&incoming(&mut stream).await?);
            reply(&mut stream, rejection, "", "").await?;
            let (mut events, _) = listener.accept().await?;
            let request = incoming(&mut events).await?;
            assert_eq!(request.method, "GET");
            assert_headers(&request);
            stream_headers(&mut events, "").await?;
            events
                .write_all(b"event: endpoint\ndata: /messages\n\n")
                .await?;
            let (mut stream, _) = listener.accept().await?;
            let request = incoming(&mut stream).await?;
            assert_eq!(
                request.body.get("method").and_then(Value::as_str),
                Some("initialize")
            );
            assert_headers(&request);
            reply(&mut stream, 202, "", "").await?;
            events.write_all(b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\"}}\n\n").await?;
            let (mut stream, _) = listener.accept().await?;
            let request = incoming(&mut stream).await?;
            assert_eq!(
                request
                    .headers
                    .get("mcp-protocol-version")
                    .map(String::as_str),
                Some("2025-03-26")
            );
            assert_eq!(
                request.body.get("method").and_then(Value::as_str),
                Some("tools/list")
            );
            reply(&mut stream, 202, "", "").await?;
            events.write_all(b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[]}}\n\n").await?;
            let mut buffer = [0u8; 1];
            use tokio::io::AsyncReadExt;
            assert_eq!(events.read(&mut buffer).await?, 0);
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        let (responses, mut receiver) = mpsc::channel(8);
        let mut handle = spawn_configured(url, false, headers(), None, None, responses).await?;
        handle.request_tx.send(super::tests::initialize(1)).await?;
        handle
            .request_tx
            .send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.into())
            .await?;
        assert_eq!(
            message(&mut receiver).await?.get("id"),
            Some(&Value::from(1))
        );
        assert_eq!(
            message(&mut receiver).await?.get("id"),
            Some(&Value::from(2))
        );
        stop(&mut handle).await?;
        server.await??;
    }
    Ok(())
}

#[tokio::test]
async fn auth_failures_and_unknown_outcomes_do_not_fallback() -> TestResult {
    for rejection in [None, Some(401), Some(500)] {
        let (listener, url) = fixture().await?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            incoming(&mut stream).await?;
            if let Some(status) = rejection {
                reply(&mut stream, status, "", "synthetic-secret-body").await?;
            }
            drop(stream);
            assert!(
                timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "must not retry or open legacy GET"
            );
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        let (responses, mut receiver) = mpsc::channel(4);
        let mut handle = spawn_configured(url, false, headers(), None, None, responses).await?;
        handle.request_tx.send(super::tests::initialize(1)).await?;
        let response = message(&mut receiver).await?;
        assert!(response.get("error").is_some());
        assert!(!response.to_string().contains("synthetic-secret"));
        stop(&mut handle).await?;
        server.await??;
    }
    Ok(())
}

#[tokio::test]
async fn credentials_are_not_sent_to_redirects_or_cross_origin_legacy_endpoints() -> TestResult {
    for legacy in [false, true] {
        let (listener, url) = fixture().await?;
        let (other, endpoint) = fixture().await?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            assert_headers(&incoming(&mut stream).await?);
            if legacy {
                stream_headers(&mut stream, "").await?;
                stream
                    .write_all(
                        format!("event: endpoint\ndata: {endpoint}?synthetic-secret-query\n\n")
                            .as_bytes(),
                    )
                    .await?;
            } else {
                reply(
                    &mut stream,
                    307,
                    &format!("Location: {endpoint}?synthetic-secret-query\r\n"),
                    "synthetic-secret-body",
                )
                .await?;
            }
            assert!(
                timeout(Duration::from_millis(100), other.accept())
                    .await
                    .is_err()
            );
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        let (responses, mut receiver) = mpsc::channel(4);
        if legacy {
            let error = spawn_configured(url, true, headers(), None, None, responses)
                .await
                .err()
                .ok_or("cross-origin endpoint accepted")?;
            assert_eq!(
                error.to_string(),
                "Legacy SSE message endpoint must use the same origin"
            );
        } else {
            let mut handle = spawn_configured(url, false, headers(), None, None, responses).await?;
            handle.request_tx.send(super::tests::initialize(1)).await?;
            let response = message(&mut receiver).await?;
            assert_eq!(
                response.pointer("/error/message").and_then(Value::as_str),
                Some("HTTP upstream returned status 307; request was not replayed")
            );
            assert!(!response.to_string().contains("synthetic-secret"));
            stop(&mut handle).await?;
        }
        server.await??;
    }
    Ok(())
}
