use super::tests::{TestResult, fixture, incoming, message, reply, stop};
use super::*;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn modern_request(
    identifier: i64,
    method: &str,
    mut parameters: Value,
) -> anyhow::Result<crate::upstream::UpstreamRequest> {
    crate::mcp_protocol::stamp(&mut parameters)?;
    Ok(json!({
        "jsonrpc":"2.0","id":identifier,"method":method,"params":parameters
    })
    .to_string()
    .into())
}

#[tokio::test]
async fn modern_discovery_and_calls_own_version_method_and_name_headers() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        for (identifier, method, name) in [
            (1, "server/discover", None),
            (2, "tools/list", None),
            (3, "tools/call", Some("echo")),
        ] {
            let (mut stream, _) = listener.accept().await?;
            let request = incoming(&mut stream).await?;
            assert_eq!(request.method, "POST");
            assert_eq!(request.body.get("method"), Some(&json!(method)));
            assert_eq!(
                request
                    .headers
                    .get("mcp-protocol-version")
                    .map(String::as_str),
                Some("2026-07-28")
            );
            assert_eq!(
                request.headers.get("mcp-method").map(String::as_str),
                Some(method)
            );
            assert_eq!(request.headers.get("mcp-name").map(String::as_str), name);
            assert!(!request.headers.contains_key("mcp-session-id"));
            assert_eq!(
                request.headers.get("x-fixture").map(String::as_str),
                Some("configured")
            );
            let result = match method {
                "server/discover" => json!({
                    "supportedVersions":["2026-07-28"],"capabilities":{"tools":{}}
                }),
                "tools/list" => json!({"tools":[],"resultType":"complete","ttlMs":0}),
                _ => json!({"content":[],"resultType":"complete"}),
            };
            reply(
                &mut stream,
                200,
                "Content-Type: application/json\r\n",
                &json!({"jsonrpc":"2.0","id":identifier,"result":result}).to_string(),
            )
            .await?;
        }
        assert!(
            timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err(),
            "stateless modern retirement sent an unexpected request"
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(8);
    let mut handle = spawn_configured(
        url,
        false,
        BTreeMap::from([
            ("MCP-Method".into(), "forged".into()),
            ("MCP-Name".into(), "forged".into()),
            ("MCP-Protocol-Version".into(), "forged".into()),
            ("X-Fixture".into(), "configured".into()),
        ]),
        None,
        None,
        responses,
    )
    .await?;
    for (identifier, method, parameters) in [
        (1, "server/discover", json!({})),
        (2, "tools/list", json!({})),
        (3, "tools/call", json!({"name":"echo","arguments":{}})),
    ] {
        handle
            .request_tx
            .send(modern_request(identifier, method, parameters)?)
            .await?;
        let response = message(&mut receiver).await?;
        assert_eq!(response.get("id"), Some(&json!(identifier)));
        assert!(response.get("result").is_some(), "{response}");
    }
    stop(&mut handle).await?;
    server.await?
}

#[tokio::test]
async fn definitive_discovery_http_mismatch_allows_same_endpoint_legacy_initialize() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut probe, _) = listener.accept().await?;
        assert_eq!(
            incoming(&mut probe).await?.body.get("method"),
            Some(&json!("server/discover"))
        );
        reply(&mut probe, 400, "", "").await?;
        let (mut initialization, _) = listener.accept().await?;
        let initialize = incoming(&mut initialization).await?;
        assert_eq!(initialize.body.get("method"), Some(&json!("initialize")));
        assert!(!initialize.headers.contains_key("mcp-method"));
        assert!(!initialize.headers.contains_key("mcp-protocol-version"));
        reply(
            &mut initialization,
            200,
            "Content-Type: application/json\r\n",
            &json!({"jsonrpc":"2.0","id":2,"result":{
                "protocolVersion":"2025-11-25","capabilities":{}
            }})
            .to_string(),
        )
        .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(8);
    let mut handle = spawn(url, false, responses).await?;
    handle
        .request_tx
        .send(modern_request(1, "server/discover", json!({}))?)
        .await?;
    let rejection = message(&mut receiver).await?;
    assert_eq!(
        rejection.pointer("/error/code"),
        Some(&json!(crate::mcp_protocol::DISCOVERY_UNSUPPORTED))
    );
    assert_eq!(
        rejection.pointer("/error/data/httpStatus"),
        Some(&json!(400))
    );
    handle.request_tx.send(tests::initialize(2)).await?;
    assert_eq!(
        message(&mut receiver)
            .await?
            .pointer("/result/protocolVersion"),
        Some(&json!("2025-11-25"))
    );
    stop(&mut handle).await?;
    server.await?
}

#[tokio::test]
async fn discovery_auth_failure_is_not_a_negotiation_fallback_signal() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut probe, _) = listener.accept().await?;
        assert_eq!(
            incoming(&mut probe).await?.body.get("method"),
            Some(&json!("server/discover"))
        );
        reply(&mut probe, 401, "", "").await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(8);
    let mut handle = spawn(url, false, responses).await?;
    handle
        .request_tx
        .send(modern_request(1, "server/discover", json!({}))?)
        .await?;
    let response = message(&mut receiver).await?;
    assert_eq!(response.pointer("/error/code"), Some(&json!(-32000)));
    assert!(
        response
            .pointer("/error/message")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("401"))
    );
    stop(&mut handle).await?;
    server.await?
}

#[tokio::test]
async fn rejected_probe_does_not_consume_initial_legacy_sse_fallback() -> TestResult {
    let (listener, url) = fixture().await?;
    let server = tokio::spawn(async move {
        let (mut probe, _) = listener.accept().await?;
        assert_eq!(
            incoming(&mut probe).await?.body.get("method"),
            Some(&json!("server/discover"))
        );
        reply(&mut probe, 404, "", "").await?;
        let (mut rejected, _) = listener.accept().await?;
        assert_eq!(
            incoming(&mut rejected).await?.body.get("method"),
            Some(&json!("initialize"))
        );
        reply(&mut rejected, 405, "", "").await?;
        let (mut events, _) = listener.accept().await?;
        assert_eq!(incoming(&mut events).await?.method, "GET");
        tests::stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\ndata: /messages\n\n")
            .await?;
        let (mut posted, _) = listener.accept().await?;
        let initialize = incoming(&mut posted).await?;
        assert_eq!(initialize.path, "/messages");
        assert_eq!(initialize.body.get("method"), Some(&json!("initialize")));
        reply(&mut posted, 202, "", "").await?;
        events
            .write_all(
                b"data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{}}}\n\n",
            )
            .await?;
        let mut closed = [0];
        assert_eq!(events.read(&mut closed).await?, 0);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(8);
    let mut handle = spawn(url, false, responses).await?;
    handle
        .request_tx
        .send(modern_request(1, "server/discover", json!({}))?)
        .await?;
    assert_eq!(
        message(&mut receiver)
            .await?
            .pointer("/error/data/httpStatus"),
        Some(&json!(404))
    );
    handle.request_tx.send(tests::initialize(2)).await?;
    assert_eq!(
        message(&mut receiver)
            .await?
            .pointer("/result/protocolVersion"),
        Some(&json!("2025-11-25"))
    );
    stop(&mut handle).await?;
    server.await?
}
