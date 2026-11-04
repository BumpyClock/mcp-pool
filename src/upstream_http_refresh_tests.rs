use super::*;
use crate::upstream_http::tests::stream_headers;

#[tokio::test]
async fn refresh_stream_counts_toward_worker_limit_and_retires_with_calls() -> TestResult {
    let (listener, url) = fixture().await?;
    let (filled, all_filled) = oneshot::channel();
    let (check, check_received) = oneshot::channel();
    let (checked, check_complete) = oneshot::channel();
    let (release, release_received) = oneshot::channel();
    let (extra_seen, extra_observed) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut initial, _) = listener.accept().await?;
        assert_eq!(
            incoming(&mut initial).await?.body.get("id"),
            Some(&json!(1))
        );
        let result = json!({"supportedVersions":["2026-07-28"],"capabilities":{},"ttlMs":0});
        reply(
            &mut initial,
            200,
            "Content-Type: application/json\r\n",
            &json!({"jsonrpc":"2.0","id":1,"result":result}).to_string(),
        )
        .await?;
        let (mut refresh, _) = listener.accept().await?;
        assert_eq!(
            incoming(&mut refresh).await?.body.get("id"),
            Some(&json!(2))
        );
        stream_headers(&mut refresh, "").await?;
        refresh
            .write_all(
                format!(
                    "data: {}\n\n",
                    json!({"jsonrpc":"2.0","id":2,"result":result})
                )
                .as_bytes(),
            )
            .await?;
        refresh.flush().await?;
        let mut calls = Vec::new();
        let mut last_identifier = Value::Null;
        for _ in 0..MAX_CONCURRENT_REQUESTS - 1 {
            let (mut call, _) = listener.accept().await?;
            let request = incoming(&mut call).await?;
            assert_eq!(request.body.get("method"), Some(&json!("tools/call")));
            last_identifier = request.body.get("id").ok_or("missing call id")?.clone();
            calls.push(call);
        }
        filled.send(()).map_err(|_| "fill observer closed")?;
        check_received.await?;
        assert!(
            timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "worker limit bypassed"
        );
        checked.send(()).map_err(|_| "check observer closed")?;
        release_received.await?;
        let mut completed = calls.pop().ok_or("missing held call")?;
        reply(
            &mut completed,
            200,
            "Content-Type: application/json\r\n",
            &json!({"jsonrpc":"2.0","id":last_identifier,"result":{}}).to_string(),
        )
        .await?;
        let (mut extra, _) = listener.accept().await?;
        assert_eq!(
            incoming(&mut extra).await?.body.get("id"),
            Some(&json!(100))
        );
        extra_seen.send(()).map_err(|_| "extra observer closed")?;
        calls.push(extra);
        calls.push(refresh);
        for mut stream in calls {
            let mut byte = [0];
            assert_eq!(
                timeout(Duration::from_secs(3), stream.read(&mut byte)).await??,
                0
            );
        }
        assert!(
            timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "shutdown replayed request"
        );
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(64);
    let mut handle =
        spawn_configured(url, false, BTreeMap::new(), Some(15000), None, responses).await?;
    handle
        .request_tx
        .send(modern_request(1, "server/discover", json!({}))?)
        .await?;
    assert_eq!(message(&mut receiver).await?.get("id"), Some(&json!(1)));
    handle
        .request_tx
        .send(modern_request(2, "server/discover", json!({}))?)
        .await?;
    assert_eq!(message(&mut receiver).await?.get("id"), Some(&json!(2)));
    for identifier in 3..(3 + MAX_CONCURRENT_REQUESTS as i64 - 1) {
        handle
            .request_tx
            .send(modern_request(
                identifier,
                "tools/call",
                json!({"name":"echo"}),
            )?)
            .await?;
    }
    timeout(Duration::from_secs(5), all_filled).await??;
    handle
        .request_tx
        .send(modern_request(100, "tools/call", json!({"name":"echo"}))?)
        .await?;
    check.send(()).map_err(|_| "check receiver closed")?;
    timeout(Duration::from_secs(3), check_complete).await??;
    release.send(()).map_err(|_| "release receiver closed")?;
    timeout(Duration::from_secs(3), extra_observed).await??;
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}
