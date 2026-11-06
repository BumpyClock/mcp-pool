use super::tests::{
    TestResult, fixture, incoming, initialize, message, reply, stop, stream_headers,
};
use super::*;
use crate::request_deadline::{SharedDeadline, TIMEOUT_FIELD};
use crate::upstream::UpstreamRequest;

#[test]
fn prepared_bodies_strip_private_deadlines_and_retain_original_shared_budget() -> TestResult {
    let options = Options::new(BTreeMap::new(), Some(600), None, ServerLog::default())?;
    let text = "x".repeat(512 * 1024);
    let raw = format!(
        " {{ \"jsonrpc\" : \"2.0\", \"id\" : 7, \"method\" : \"tools/call\", \"params\" : {{\"name\":\"echo\",\"arguments\":{{\"text\":\"{text}\"}}}} }} \n"
    );
    let request = Request::parse(raw.clone().into(), &options).map_err(|(_, error)| error)?;
    assert_eq!(request.line, raw);
    assert_eq!(request.identifier, Some(Value::from(7)));
    let deadline = std::time::Instant::now() + Duration::from_secs(1);
    let budget = SharedDeadline::new(deadline);
    let original = serde_json::json!({
        "jsonrpc":"2.0","id":"init","method":"initialize",
        "params":{"protocolVersion":"2025-03-26","extension":{"text":text}},
        TIMEOUT_FIELD:5
    });
    let request = Request::parse(
        UpstreamRequest {
            line: original.to_string(),
            deadline: Some(budget.clone()),
        },
        &options,
    )
    .map_err(|(_, error)| error)?;
    assert_eq!(request.read_deadline(&options), Duration::from_millis(600));
    let body: Value = serde_json::from_str(&request.line)?;
    assert_eq!(body.get(TIMEOUT_FIELD), None);
    assert_eq!(body.get("params"), original.get("params"));
    let extended = deadline + Duration::from_secs(1);
    budget.extend(extended);
    assert_eq!(request.budget.current(), extended);
    Ok(())
}

#[test]
fn parse_failures_return_the_original_envelope_for_error_correlation() -> TestResult {
    let options = Options::new(BTreeMap::new(), None, None, ServerLog::default())?;
    for (line, expected) in [
        ("not-json".to_owned(), "HTTP request is not valid JSON"),
        ("[1,2,3]".to_owned(), "HTTP transport requires a JSON-RPC object"),
        (r#" {"jsonrpc":"2.0","id":"client","method":"tools/call","_mcp_pool_timeout_ms":null} "#.to_owned(), "Pool request timeout must be positive u64 milliseconds"),
        (format!(r#"{{"id":-1,"text":"{}"}}"#, "x".repeat(1024 * 1024)), "HTTP request exceeds size limit"),
    ] {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        let parsed = Request::parse(UpstreamRequest {
            line: line.clone(), deadline: Some(SharedDeadline::new(deadline))
        }, &options);
        let (envelope, error) = parsed.err().ok_or("expected parsing error")?;
        assert_eq!(error.to_string(), expected);
        assert_eq!(envelope.line, line);
        assert_eq!(envelope.deadline.ok_or("missing original budget")?.current(), deadline);
    }
    Ok(())
}

#[tokio::test]
async fn large_http_call_moves_body_and_preserves_correlated_result() -> TestResult {
    let (listener, url) = fixture().await?;
    let arguments =
        serde_json::json!({"text":"x".repeat(512 * 1024),"nested":[null,1,{"flag":true}]});
    let expected = arguments.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        incoming(&mut stream).await?;
        reply(
            &mut stream,
            200,
            "Content-Type: application/json\r\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#,
        )
        .await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(request.body.get(TIMEOUT_FIELD), None);
        assert_eq!(request.body.pointer("/params/arguments"), Some(&expected));
        reply(
            &mut stream,
            200,
            "Content-Type: application/json\r\n",
            &serde_json::json!({
                "jsonrpc":"2.0","id":"large-call","result":{"arguments":expected}
            })
            .to_string(),
        )
        .await?;
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle = spawn(url, false, responses).await?;
    handle.request_tx.send(initialize(1)).await?;
    message(&mut receiver).await?;
    handle
        .request_tx
        .send(
            serde_json::json!({
                "jsonrpc":"2.0","id":"large-call","method":"tools/call",
                "params":{"name":"echo","arguments":arguments}, TIMEOUT_FIELD:5000
            })
            .to_string()
            .into(),
        )
        .await?;
    let result = message(&mut receiver).await?;
    assert_eq!(result.get("id"), Some(&Value::from("large-call")));
    assert_eq!(result.pointer("/result/arguments"), Some(&arguments));
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn permitted_fallback_preserves_large_initialize_and_never_replays_a_later_rejection()
-> TestResult {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (listener, url) = fixture().await?;
    let initialize = serde_json::json!({
        "jsonrpc":"2.0","id":"init","method":"initialize",
        "params":{"protocolVersion":"2025-03-26","capabilities":{},
            "clientInfo":{"name":"fixture","version":"1"},"extension":"x".repeat(256 * 1024)}
    });
    let expected = initialize.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(request.method, "POST");
        assert_eq!(request.body, expected);
        reply(&mut stream, 405, "", "").await?;
        let (mut events, _) = listener.accept().await?;
        assert_eq!(incoming(&mut events).await?.method, "GET");
        stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\ndata: /messages\n\n")
            .await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(request.method, "POST");
        assert_eq!(request.body, expected);
        reply(&mut stream, 202, "", "").await?;
        events.write_all(b"data: {\"jsonrpc\":\"2.0\",\"id\":\"init\",\"result\":{\"protocolVersion\":\"2025-03-26\"}}\n\n").await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(request.body.get("method"), Some(&Value::from("tools/call")));
        reply(&mut stream, 405, "", "").await?;
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        let mut buffer = [0u8; 1];
        assert_eq!(events.read(&mut buffer).await?, 0);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle = spawn(url, false, responses).await?;
    let mut private = initialize;
    private
        .as_object_mut()
        .ok_or("initialize object")?
        .insert(TIMEOUT_FIELD.to_owned(), Value::from(5000));
    handle.request_tx.send(private.to_string().into()).await?;
    assert_eq!(
        message(&mut receiver).await?.get("id"),
        Some(&Value::from("init"))
    );
    handle
        .request_tx
        .send(
            r#"{"jsonrpc":"2.0","id":"later","method":"tools/call","params":{"name":"echo"}}"#
                .into(),
        )
        .await?;
    let error = message(&mut receiver).await?;
    assert_eq!(error.get("id"), Some(&Value::from("later")));
    assert_eq!(
        error.pointer("/error/message"),
        Some(&Value::from(
            "Legacy SSE POST returned status 405; request was not replayed"
        ))
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}
