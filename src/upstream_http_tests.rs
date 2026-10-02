use std::collections::HashMap;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;

pub(super) type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub(super) struct Incoming {
    pub method: String,
    pub path: String,
    pub headers: HashMap<String, String>,
    pub body: Value,
}

pub(super) async fn fixture() -> io::Result<(TcpListener, String)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/mcp", listener.local_addr()?);
    Ok((listener, url))
}

pub(super) async fn incoming(
    stream: &mut TcpStream,
) -> Result<Incoming, Box<dyn std::error::Error + Send + Sync>> {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).await?;
        bytes.extend_from_slice(&byte);
        if bytes.len() > 16384 {
            return Err("fixture HTTP header is too large".into());
        }
    }
    let header = std::str::from_utf8(&bytes)?;
    let mut lines = header.lines();
    let mut start = lines
        .next()
        .ok_or("missing request line")?
        .split_whitespace();
    let method = start.next().ok_or("missing method")?.to_string();
    let path = start.next().ok_or("missing path")?.to_string();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    let length: usize = headers
        .get("content-length")
        .map(String::as_str)
        .unwrap_or("0")
        .parse()?;
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await?;
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body)?
    };
    Ok(Incoming {
        method,
        path,
        headers,
        body,
    })
}

pub(super) async fn reply(
    stream: &mut TcpStream,
    status: u16,
    headers: &str,
    body: &str,
) -> io::Result<()> {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} Fixture\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    stream.flush().await
}

pub(super) async fn stream_headers(stream: &mut TcpStream, headers: &str) -> io::Result<()> {
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\n{headers}\r\n"
            )
            .as_bytes(),
        )
        .await
}

pub(super) async fn message(
    receiver: &mut mpsc::Receiver<String>,
) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
    let line = timeout(Duration::from_secs(3), receiver.recv())
        .await?
        .ok_or("upstream response channel closed")?;
    Ok(serde_json::from_str(&line)?)
}

pub(super) async fn stop(handle: &mut UpstreamHandle) -> TestResult {
    timeout(Duration::from_secs(1), handle.shutdown()).await??;
    Ok(())
}

pub(super) fn initialize(identifier: i64) -> String {
    serde_json::json!({
        "jsonrpc":"2.0", "id":identifier, "method":"initialize",
        "params":{"protocolVersion":"2025-03-26", "capabilities":{},
        "clientInfo":{"name":"fixture","version":"1"}}
    })
    .to_string()
}

#[tokio::test]
async fn session_and_negotiated_version_precede_queued_requests() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/mcp");
        assert_eq!(
            request.headers.get("accept").map(String::as_str),
            Some("application/json, text/event-stream")
        );
        assert!(!request.headers.contains_key("mcp-session-id"));
        reply(
            &mut stream,
            200,
            "Content-Type: application/json\r\nMcp-Session-Id: session-one\r\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{}}}"#,
        )
        .await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(
            request.headers.get("mcp-session-id").map(String::as_str),
            Some("session-one")
        );
        assert_eq!(
            request
                .headers
                .get("mcp-protocol-version")
                .map(String::as_str),
            Some("2024-11-05")
        );
        assert_eq!(
            request.body.get("id"),
            Some(&Value::String("rewritten".into()))
        );
        reply(
            &mut stream,
            200,
            "Content-Type: application/json; charset=utf-8\r\n",
            r#"{"jsonrpc":"2.0","id":"rewritten","result":{}}"#,
        )
        .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, false, response_tx).await?;
    handle.request_tx.send(initialize(1)).await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":"rewritten","method":"tools/list"}"#.into())
        .await?;
    assert_eq!(
        message(&mut responses).await?.get("id"),
        Some(&Value::from(1))
    );
    assert_eq!(
        message(&mut responses).await?.get("id"),
        Some(&Value::String("rewritten".into()))
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn incremental_sse_dispatch_and_concurrency_before_eof() -> TestResult {
    let (listener, url) = fixture().await?;
    let (release, released) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await?;
        incoming(&mut first).await?;
        stream_headers(&mut first, "").await?;
        first
            .write_all(b": heartbeat\r\ndata: {\"jsonrpc\":\"2.0\",\r\ndata: \"method\":\"notifications/progress\"}\r")
            .await?;
        first.write_all(b"\n\r\n").await?;
        first.flush().await?;
        let (mut second, _) = listener.accept().await?;
        incoming(&mut second).await?;
        reply(
            &mut second,
            200,
            "Content-Type: application/json\r\n",
            r#"{"jsonrpc":"2.0","id":8,"result":{}}"#,
        )
        .await?;
        released.await?;
        first
            .write_all(
                b"event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\r\n\r\n",
            )
            .await?;
        first.flush().await?;
        let mut byte = [0];
        assert_eq!(first.read(&mut byte).await?, 0);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, false, response_tx).await?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":7,"method":"tools/call"}"#.into())
        .await?;
    assert_eq!(
        message(&mut responses)
            .await?
            .get("method")
            .and_then(Value::as_str),
        Some("notifications/progress")
    );
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":8,"method":"tools/list"}"#.into())
        .await?;
    assert_eq!(
        message(&mut responses).await?.get("id"),
        Some(&Value::from(8))
    );
    release.send(()).map_err(|_| "fixture release closed")?;
    assert_eq!(
        message(&mut responses).await?.get("id"),
        Some(&Value::from(7))
    );
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn failures_preserve_rewritten_ids_without_replaying() -> TestResult {
    for (status, content, body) in [
        (500, "application/json", r#"{"private":"do not expose"}"#),
        (302, "application/json", ""),
        (202, "application/json", ""),
        (200, "application/json", "not json"),
        (
            200,
            "application/json",
            r#"{"jsonrpc":"2.0","id":99,"result":{}}"#,
        ),
        (200, "text/event-stream", "data: invalid\r\n\r\n"),
        (200, "text/html", "private data"),
    ] {
        let (listener, url) = fixture().await?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            incoming(&mut stream).await?;
            reply(
                &mut stream,
                status,
                &format!("Content-Type: {content}\r\n"),
                body,
            )
            .await?;
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
            .send(r#"{"jsonrpc":"2.0","id":"client-5","method":"tools/call"}"#.into())
            .await?;
        let mut response = message(&mut responses).await?;
        if response.get("id") == Some(&Value::from(99)) {
            response = message(&mut responses).await?;
        }
        assert_eq!(response.get("id"), Some(&Value::String("client-5".into())));
        assert_eq!(response.pointer("/error/code"), Some(&Value::from(-32000)));
        assert!(!response.to_string().contains("private"));
        stop(&mut handle).await?;
        server.await??;
    }
    Ok(())
}

#[tokio::test]
async fn network_failure_preserves_numeric_id() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        drop(stream);
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
        .send(r#"{"jsonrpc":"2.0","id":44,"method":"tools/call"}"#.into())
        .await?;
    let response = message(&mut responses).await?;
    assert_eq!(response.get("id"), Some(&Value::from(44)));
    assert!(response.get("error").is_some());
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn empty_notification_responses_are_not_errors() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        for status in [202, 204, 204] {
            let (mut stream, _) = listener.accept().await?;
            let request = incoming(&mut stream).await?;
            assert!(request.body.get("method").is_none() || request.body.get("id").is_none());
            reply(&mut stream, status, "", "").await?;
        }
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, false, response_tx).await?;
    for _ in 0..2 {
        handle
            .request_tx
            .send(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.into())
            .await?;
    }
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":"server-request","result":{}}"#.into())
        .await?;
    server.await??;
    assert!(
        timeout(Duration::from_millis(100), responses.recv())
            .await
            .is_err()
    );
    stop(&mut handle).await?;
    Ok(())
}

#[tokio::test]
async fn expired_sessions_reject_later_requests_without_replay() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        reply(
            &mut stream,
            200,
            "Content-Type: application/json\r\nMcp-Session-Id: expires\r\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#,
        )
        .await?;
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        reply(&mut stream, 404, "", "").await?;
        assert!(
            timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err()
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, false, response_tx).await?;
    handle.request_tx.send(initialize(1)).await?;
    message(&mut responses).await?;
    for identifier in [2, 3] {
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

#[tokio::test]
async fn shutdown_cancels_pending_network_io_and_confirms_completion() -> TestResult {
    let (listener, url) = fixture().await?;
    let (accepted_tx, accepted_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        accepted_tx
            .send(())
            .map_err(|_| "fixture accept waiter closed")?;
        let mut byte = [0];
        assert_eq!(
            timeout(Duration::from_secs(2), stream.read(&mut byte)).await??,
            0
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, _responses) = mpsc::channel(16);
    let mut handle = spawn(url, false, response_tx).await?;
    handle.request_tx.send(initialize(1)).await?;
    timeout(Duration::from_secs(2), accepted_rx).await??;
    stop(&mut handle).await?;
    handle.wait_for_exit().await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn legacy_sse_discovers_endpoint_and_receives_post_responses_over_get() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut events, _) = listener.accept().await?;
        let request = incoming(&mut events).await?;
        assert_eq!(request.method, "GET");
        stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\r\ndata: /messages?sessionId=legacy\r\n\r\n")
            .await?;
        events.flush().await?;
        let (mut post, _) = listener.accept().await?;
        let request = incoming(&mut post).await?;
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/messages?sessionId=legacy");
        reply(&mut post, 202, "", "").await?;
        events.write_all(b"event: message\r\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2024-11-05\"}}\r\n\r\n").await?;
        events.flush().await?;
        let (mut post, _) = listener.accept().await?;
        let request = incoming(&mut post).await?;
        assert_eq!(
            request
                .headers
                .get("mcp-protocol-version")
                .map(String::as_str),
            Some("2024-11-05")
        );
        reply(&mut post, 202, "", "").await?;
        events
            .write_all(b"data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n\n")
            .await?;
        events.flush().await?;
        let mut byte = [0];
        assert_eq!(events.read(&mut byte).await?, 0);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (response_tx, mut responses) = mpsc::channel(16);
    let mut handle = spawn(url, true, response_tx).await?;
    handle.request_tx.send(initialize(1)).await?;
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
