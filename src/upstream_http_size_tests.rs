use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::tests::*;
use super::*;

#[tokio::test]
async fn tool_catalog_larger_than_one_mib_survives_json_and_sse_transports() -> TestResult {
    for (legacy, event_stream) in [(false, false), (false, true), (true, true)] {
        let (listener, url) = fixture().await?;
        let tools = serde_json::json!([{
            "name":"large_catalog_tool",
            "description":"x".repeat(2 * 1024 * 1024),
            "inputSchema":{"type":"object"}
        }]);
        let expected = tools.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await?;
            let request = incoming(&mut stream).await?;
            if legacy {
                assert_eq!(request.method, "GET");
                stream_headers(&mut stream, "").await?;
                stream
                    .write_all(b"event: endpoint\ndata: /messages\n\n")
                    .await?;
                stream.flush().await?;
                let (mut post, _) = listener.accept().await?;
                let request = incoming(&mut post).await?;
                assert_eq!(request.body.get("method"), Some(&Value::from("tools/list")));
                reply(&mut post, 202, "", "").await?;
            } else {
                assert_eq!(request.body.get("method"), Some(&Value::from("tools/list")));
            }
            let body = serde_json::json!({
                "jsonrpc":"2.0", "id":"catalog", "result":{"tools":tools}
            })
            .to_string();
            if legacy {
                stream
                    .write_all(format!("event: message\ndata: {body}\n\n").as_bytes())
                    .await?;
                stream.flush().await?;
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).await?, 0);
            } else if event_stream {
                reply(
                    &mut stream,
                    200,
                    "Content-Type: text/event-stream\r\n",
                    &format!("event: message\ndata: {body}\n\n"),
                )
                .await?;
            } else {
                reply(
                    &mut stream,
                    200,
                    "Content-Type: application/json\r\n",
                    &body,
                )
                .await?;
            }
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        });
        let (response_tx, mut responses) = mpsc::channel(4);
        let mut handle = spawn(url, legacy, response_tx).await?;
        handle
            .request_tx
            .send(r#"{"jsonrpc":"2.0","id":"catalog","method":"tools/list"}"#.into())
            .await?;
        let response = message(&mut responses).await?;
        assert_eq!(response.get("id"), Some(&Value::from("catalog")));
        assert!(
            response.pointer("/result/tools") == Some(&expected),
            "catalog was not preserved (legacy={legacy}, event_stream={event_stream}); error={:?}",
            response.get("error")
        );
        stop(&mut handle).await?;
        server.await??;
    }
    Ok(())
}
