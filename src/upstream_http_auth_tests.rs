use super::tests::{
    TestResult, fixture, incoming, initialize, message, reply, stop, stream_headers,
};
use super::*;
use crate::config::ServerDef;
use crate::server_config::ConfiguredServer;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Store(PathBuf);

impl Store {
    fn new() -> io::Result<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::current_dir()?.join(format!(
            ".transport-auth-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn server(&self, url: &str) -> ConfiguredServer {
        let location = self.0.to_string_lossy().into_owned();
        ConfiguredServer {
            name: "synthetic-transport".into(),
            definition: ServerDef {
                url: url.into(),
                env: BTreeMap::from([
                    ("HOME".into(), location.clone()),
                    ("XDG_DATA_HOME".into(), location),
                ]),
                ..Default::default()
            },
            source: self.0.join("fixture.json"),
            raw: serde_json::json!({"auth":"oauth"}),
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            crate::diagnostics::log(format!("transport_auth_fixture_cleanup_failed: {error}"));
        }
    }
}

fn save(server: &ConfiguredServer, token: &str) -> anyhow::Result<()> {
    crate::oauth::vault_set(
        server,
        &serde_json::json!({"access_token":token,"token_type":"Bearer"}),
    )
}

fn assert_authorization(request: &super::tests::Incoming, token: &str) {
    assert_eq!(
        request.headers.get("authorization"),
        Some(&format!("Bearer {token}"))
    );
    assert_eq!(
        request.headers.get("x-api-key").map(String::as_str),
        Some("synthetic-key")
    );
}

fn static_headers() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            "aUtHoRiZaTiOn".into(),
            "Bearer synthetic-static-must-not-be-used".into(),
        ),
        ("X-Api-Key".into(), "synthetic-key".into()),
    ])
}

#[tokio::test]
async fn dynamic_authorization_overrides_static_and_rereads_tokens_for_post_and_delete()
-> TestResult {
    let store = Store::new()?;
    let (listener, url) = fixture().await?;
    let definition = store.server(&url);
    save(&definition, "synthetic-generation-one")?;
    let prepared = crate::oauth::prepare(&definition, false).await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        assert_authorization(&incoming(&mut stream).await?, "synthetic-generation-one");
        reply(
            &mut stream,
            200,
            "Content-Type: application/json\r\nMcp-Session-Id: synthetic-session\r\n",
            r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26"}}"#,
        )
        .await?;
        let (mut stream, _) = listener.accept().await?;
        assert_authorization(&incoming(&mut stream).await?, "synthetic-generation-two");
        reply(
            &mut stream,
            200,
            "Content-Type: application/json\r\n",
            r#"{"jsonrpc":"2.0","id":2,"result":{}}"#,
        )
        .await?;
        let (mut stream, _) = listener.accept().await?;
        let request = incoming(&mut stream).await?;
        assert_eq!(request.method, "DELETE");
        assert_authorization(&request, "synthetic-generation-two");
        reply(&mut stream, 200, "", "").await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle =
        spawn_configured(url, false, static_headers(), None, prepared.auth, responses).await?;
    handle.request_tx.send(initialize(1)).await?;
    message(&mut receiver).await?;
    save(&definition, "synthetic-generation-two")?;
    handle
        .request_tx
        .send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#.into())
        .await?;
    message(&mut receiver).await?;
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn legacy_get_and_post_obtain_current_dynamic_authorization() -> TestResult {
    let store = Store::new()?;
    let (listener, url) = fixture().await?;
    let definition = store.server(&url);
    save(&definition, "synthetic-generation-one")?;
    let prepared = crate::oauth::prepare(&definition, false).await?;
    let server = tokio::spawn(async move {
        let (mut events, _) = listener.accept().await?;
        let request = incoming(&mut events).await?;
        assert_eq!(request.method, "GET");
        assert_authorization(&request, "synthetic-generation-one");
        stream_headers(&mut events, "").await?;
        events
            .write_all(b"event: endpoint\ndata: /messages\n\n")
            .await?;
        let (mut stream, _) = listener.accept().await?;
        assert_authorization(&incoming(&mut stream).await?, "synthetic-generation-two");
        reply(&mut stream, 202, "", "").await?;
        events.write_all(b"event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-03-26\"}}\n\n").await?;
        let mut buffer = [0u8; 1];
        assert_eq!(events.read(&mut buffer).await?, 0);
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    });
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle =
        spawn_configured(url, true, static_headers(), None, prepared.auth, responses).await?;
    save(&definition, "synthetic-generation-two")?;
    handle.request_tx.send(initialize(1)).await?;
    message(&mut receiver).await?;
    stop(&mut handle).await?;
    server.await??;
    Ok(())
}

#[tokio::test]
async fn auth_origin_mismatch_and_missing_credentials_fail_without_secret_leakage() -> TestResult {
    let store = Store::new()?;
    let (listener, url) = fixture().await?;
    let definition = store.server(&url);
    let auth = crate::oauth::context(&definition)?;
    let (responses, mut receiver) = mpsc::channel(4);
    let mut handle = spawn_configured(
        url.clone(),
        false,
        static_headers(),
        None,
        Some(auth.clone()),
        responses.clone(),
    )
    .await?;
    handle.request_tx.send(initialize(1)).await?;
    let response = message(&mut receiver).await?;
    assert_eq!(
        response.pointer("/error/message").and_then(Value::as_str),
        Some("HTTP authorization could not be refreshed")
    );
    assert!(!response.to_string().contains("synthetic"));
    assert!(
        timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
    stop(&mut handle).await?;
    let error = spawn_configured(
        "http://127.0.0.1:1/mcp".into(),
        false,
        static_headers(),
        None,
        Some(auth),
        responses,
    )
    .await
    .err()
    .ok_or("auth origin mismatch was accepted")?;
    assert_eq!(
        error.to_string(),
        "HTTP authorization origin does not match upstream"
    );
    Ok(())
}

#[tokio::test]
async fn serialized_auth_policy_errors_are_actionable_after_tokens_expire() -> TestResult {
    for legacy in [false, true] {
        for cached_only in [false, true] {
            let store = Store::new()?;
            let (listener, url) = fixture().await?;
            let definition = store.server(&url);
            save(&definition, "synthetic-generation-one")?;
            let mut auth = crate::oauth::prepare(&definition, false)
                .await?
                .auth
                .ok_or("synthetic auth context missing")?;
            auth.read_only = !cached_only;
            auth.cached_only = cached_only;
            let encoded = serde_json::to_string(&auth)?;
            assert!(!encoded.contains("synthetic-generation-one"));
            let auth = serde_json::from_str(&encoded)?;
            let server = tokio::spawn(async move {
                let mut events = if legacy {
                    let (mut events, _) = listener.accept().await?;
                    assert_authorization(&incoming(&mut events).await?, "synthetic-generation-one");
                    stream_headers(&mut events, "").await?;
                    events
                        .write_all(b"event: endpoint\ndata: /messages\n\n")
                        .await?;
                    Some(events)
                } else {
                    None
                };
                for (identifier, token) in [
                    (1, "synthetic-generation-one"),
                    (3, "synthetic-generation-two"),
                ] {
                    let (mut stream, _) = listener.accept().await?;
                    let request = incoming(&mut stream).await?;
                    assert_eq!(
                        request.body.get("id"),
                        Some(&Value::from(identifier)),
                        "expired guarded credentials must not reach the server"
                    );
                    assert_authorization(&request, token);
                    let response = serde_json::json!({"jsonrpc":"2.0","id":identifier,"result":{}});
                    if let Some(events) = events.as_mut() {
                        reply(&mut stream, 202, "", "").await?;
                        events
                            .write_all(format!("event: message\ndata: {response}\n\n").as_bytes())
                            .await?;
                    } else {
                        reply(
                            &mut stream,
                            200,
                            "Content-Type: application/json\r\n",
                            &response.to_string(),
                        )
                        .await?;
                    }
                }
                if let Some(mut events) = events {
                    let mut buffer = [0u8; 1];
                    assert_eq!(events.read(&mut buffer).await?, 0);
                }
                Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
            });
            let (responses, mut receiver) = mpsc::channel(4);
            let mut handle =
                spawn_configured(url, legacy, static_headers(), None, Some(auth), responses)
                    .await?;
            handle
                .request_tx
                .send(r#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#.into())
                .await?;
            assert!(message(&mut receiver).await?.get("result").is_some());
            crate::oauth::vault_set(
                &definition,
                &serde_json::json!({"access_token":"synthetic-expired-secret","token_type":"Bearer","expires_at":1}),
            )?;
            handle
                .request_tx
                .send(r#"{"jsonrpc":"2.0","id":2,"method":"tools/call"}"#.into())
                .await?;
            let response = message(&mut receiver).await?;
            let expected = if cached_only {
                "HTTP authorization unavailable: --no-oauth permits valid cached credentials only; run `mcp-pool auth SERVER` explicitly before retrying"
            } else {
                "HTTP authorization unavailable: read-only policy blocks refresh; remove MCP_POOL_CREDENTIALS_READ_ONLY only for explicitly authorized `mcp-pool auth SERVER`"
            };
            assert_eq!(
                response.pointer("/error/message").and_then(Value::as_str),
                Some(expected)
            );
            assert!(!response.to_string().contains("synthetic"));
            save(&definition, "synthetic-generation-two")?;
            handle
                .request_tx
                .send(r#"{"jsonrpc":"2.0","id":3,"method":"tools/call"}"#.into())
                .await?;
            assert!(message(&mut receiver).await?.get("result").is_some());
            stop(&mut handle).await?;
            server.await??;
        }
    }
    Ok(())
}

#[tokio::test]
async fn legacy_discovery_preserves_safe_auth_policy_guidance() -> TestResult {
    for cached_only in [false, true] {
        let store = Store::new()?;
        let (listener, url) = fixture().await?;
        let definition = store.server(&url);
        crate::oauth::vault_set(
            &definition,
            &serde_json::json!({"access_token":"synthetic-expired-secret","token_type":"Bearer","expires_at":1}),
        )?;
        let mut auth = crate::oauth::context(&definition)?;
        auth.read_only = !cached_only;
        auth.cached_only = cached_only;
        let (responses, _receiver) = mpsc::channel(4);
        let error = spawn_configured(url, true, static_headers(), None, Some(auth), responses)
            .await
            .err()
            .ok_or("expired guarded credentials allowed SSE discovery")?;
        let message = error.to_string();
        let expected = if cached_only {
            "--no-oauth permits valid cached credentials only"
        } else {
            "read-only policy blocks refresh"
        };
        assert!(message.contains(expected), "{message}");
        assert!(message.contains("mcp-pool auth SERVER"));
        assert!(!message.contains("synthetic"));
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    }
    Ok(())
}
