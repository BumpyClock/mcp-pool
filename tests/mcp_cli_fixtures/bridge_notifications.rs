use std::fs::OpenOptions;
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::time::{sleep, timeout};

use super::support::{Fixture, parse_json};

const MODERN_VERSION: &str = "2026-07-28";
const CALL_TIMEOUT_MS: u64 = 600;

#[tokio::test]
async fn pooled_http_notifications_broadcast_survive_idle_and_cancel_on_disconnect()
-> io::Result<()> {
    let fixture = Fixture::new().await?;
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    let server = configuration
        .pointer_mut("/mcpServers/fixture")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("missing fixture definition"))?;
    server.insert(
        "args".into(),
        json!([
            "--exact",
            "bridge_notifications::notification_upstream_fixture",
            "--nocapture",
            "--quiet"
        ]),
    );
    server.insert("timeoutMs".into(), json!(CALL_TIMEOUT_MS));
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    fixture.warm("fixture").await?;
    wait_for_connections(&fixture, 0).await?;

    let mut bridge = fixture.spawn(&["serve", "--http", "0", "--servers", "fixture"])?;
    let mut diagnostics = BufReader::new(
        bridge
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("bridge stderr missing"))?,
    );
    let url = timeout(Duration::from_secs(10), async {
        loop {
            let mut line = String::new();
            if diagnostics.read_line(&mut line).await? == 0 {
                return Err(io::Error::other("bridge exited before listening"));
            }
            if let Some(url) = line.strip_prefix("mcp-pool MCP bridge listening at ") {
                return Ok(url.trim().to_owned());
            }
        }
    })
    .await
    .map_err(io::Error::other)??;
    assert!(bridge.try_wait()?.is_none());
    wait_for_connections(&fixture, 1).await?;

    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .build()
        .map_err(io::Error::other)?;
    let mut first = listen(&client, &url, "aggregate-listener").await?;
    let mut second = listen(&client, &format!("{url}/fixture"), "single-server-listener").await?;
    for (stream, identifier) in [
        (&mut first, "aggregate-listener"),
        (&mut second, "single-server-listener"),
    ] {
        assert_eq!(
            stream.message().await?,
            json!({
                "jsonrpc":"2.0",
                "method":"notifications/subscriptions/acknowledged",
                "params":{"notifications":{"toolsListChanged":true}},
                "_meta":{"io.modelcontextprotocol/subscriptionId":identifier}
            })
        );
    }
    wait_for_connections(&fixture, 3).await?;

    // A timed-out tool request retires only the ordinary call connection.
    let failed = call(
        &client,
        &url,
        "timeout",
        "fixture__delayed",
        json!({"label":"late","delay_ms":CALL_TIMEOUT_MS * 3}),
    )
    .await?;
    assert_eq!(failed.pointer("/error/code"), Some(&json!(-32603)));
    wait_for_connections(&fixture, 2).await?;
    let recovered = call(
        &client,
        &url,
        "recover",
        "fixture__echo",
        json!({"label":"recovered"}),
    )
    .await?;
    assert_eq!(
        recovered.pointer("/result/structuredContent/arguments/label"),
        Some(&json!("recovered"))
    );
    wait_for_connections(&fixture, 3).await?;

    sleep(Duration::from_millis(CALL_TIMEOUT_MS * 3)).await;
    wait_for_connections(&fixture, 3).await?;
    let emitted = call(&client, &url, "emit", "fixture__notify", json!({})).await?;
    assert_eq!(emitted.get("result"), Some(&json!({"content":[]})));
    for (stream, identifier) in [
        (&mut first, "aggregate-listener"),
        (&mut second, "single-server-listener"),
    ] {
        assert_eq!(
            stream.message().await?,
            json!({
                "jsonrpc":"2.0",
                "method":"notifications/tools/list_changed",
                "params":{"fixture":"one-upstream-broadcast"},
                "_meta":{"io.modelcontextprotocol/subscriptionId":identifier}
            })
        );
    }
    assert_eq!(fixture.event_count("spawn").await?, 1);
    assert_eq!(fixture.event_count("initialize").await?, 1);
    assert_eq!(fixture.event_count("notification").await?, 1);
    assert_eq!(fixture.event_count("tools/call").await?, 3);

    drop(first);
    wait_for_connections(&fixture, 2).await?;
    drop(second);
    wait_for_connections(&fixture, 1).await?;
    sleep(Duration::from_millis(CALL_TIMEOUT_MS * 2)).await;
    wait_for_connections(&fixture, 1).await?;
    bridge.kill().await?;
    bridge.wait().await?;
    let mut remaining_diagnostics = String::new();
    timeout(
        Duration::from_secs(5),
        diagnostics.read_to_string(&mut remaining_diagnostics),
    )
    .await
    .map_err(io::Error::other)??;
    assert!(
        !remaining_diagnostics.contains("notification listener"),
        "dedicated notification connections churned: {remaining_diagnostics}"
    );
    wait_for_connections(&fixture, 0).await?;
    fixture.finish().await
}

async fn wait_for_connections(fixture: &Fixture, expected: u64) -> io::Result<()> {
    timeout(Duration::from_secs(5), async {
        loop {
            let status = parse_json(&fixture.success(&["pool", "status", "--json"]).await?)?;
            assert_eq!(status.get("server_count"), Some(&json!(1)));
            let count = status
                .pointer("/servers/0/connection_count")
                .and_then(Value::as_u64)
                .ok_or_else(|| io::Error::other("pool status missing connection count"))?;
            if count == expected {
                return Ok(());
            }
            sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|error| {
        io::Error::other(format!(
            "pooled connection count did not reach {expected}: {error}"
        ))
    })?
}

fn metadata() -> Value {
    json!({
        "io.modelcontextprotocol/protocolVersion":MODERN_VERSION,
        "io.modelcontextprotocol/clientInfo":{"name":"pooled-bridge-test","version":"1"},
        "io.modelcontextprotocol/clientCapabilities":{}
    })
}

async fn listen(client: &reqwest::Client, url: &str, identifier: &str) -> io::Result<SseResponse> {
    let response = timeout(
        Duration::from_secs(5),
        client
            .post(url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", MODERN_VERSION)
            .header("mcp-method", "subscriptions/listen")
            .json(&json!({
                "jsonrpc":"2.0","id":identifier,"method":"subscriptions/listen",
                "params":{"_meta":metadata(),"notifications":{"toolsListChanged":true}}
            }))
            .send(),
    )
    .await
    .map_err(io::Error::other)?
    .map_err(io::Error::other)?;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get("content-type")
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream; charset=utf-8")
    );
    Ok(SseResponse {
        response,
        pending: String::new(),
    })
}

async fn call(
    client: &reqwest::Client,
    url: &str,
    identifier: &str,
    name: &str,
    arguments: Value,
) -> io::Result<Value> {
    timeout(Duration::from_secs(5), async {
        let response = client
            .post(url)
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", MODERN_VERSION)
            .header("mcp-method", "tools/call")
            .header("mcp-name", name)
            .json(&json!({
                "jsonrpc":"2.0","id":identifier,"method":"tools/call",
                "params":{"_meta":metadata(),"name":name,"arguments":arguments}
            }))
            .send()
            .await
            .map_err(io::Error::other)?;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response.json::<Value>().await.map_err(io::Error::other)?;
        assert_eq!(body.get("id"), Some(&json!(identifier)));
        Ok(body)
    })
    .await
    .map_err(io::Error::other)?
}

struct SseResponse {
    response: reqwest::Response,
    pending: String,
}

impl SseResponse {
    async fn message(&mut self) -> io::Result<Value> {
        timeout(Duration::from_secs(5), async {
            loop {
                while let Some(end) = self.pending.find("\n\n") {
                    let frame = self.pending.drain(..end + 2).collect::<String>();
                    if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data: ")) {
                        return serde_json::from_str(data).map_err(io::Error::other);
                    }
                }
                let chunk = self
                    .response
                    .chunk()
                    .await
                    .map_err(io::Error::other)?
                    .ok_or_else(|| io::Error::other("SSE closed before its next message"))?;
                self.pending
                    .push_str(std::str::from_utf8(&chunk).map_err(io::Error::other)?);
            }
        })
        .await
        .map_err(io::Error::other)?
    }
}

fn record(event: &str) -> io::Result<()> {
    let path = std::env::var_os(super::stdio::COUNTER)
        .ok_or_else(|| io::Error::other("notification fixture counter missing"))?;
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(format!("{event}\n").as_bytes())
}

#[test]
fn notification_upstream_fixture() -> io::Result<()> {
    if std::env::var_os(super::stdio::MARKER).is_none() {
        return Ok(());
    }
    record("spawn")?;
    let output = Arc::new(Mutex::new(io::stdout()));
    let mut workers = Vec::new();
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line?).map_err(io::Error::other)?;
        if request.get("id").is_none() {
            continue;
        }
        let output = Arc::clone(&output);
        workers.push(std::thread::spawn(move || -> io::Result<()> {
            let notification = request.get("method") == Some(&json!("tools/call"))
                && request.pointer("/params/name") == Some(&json!("notify"));
            let response = if notification {
                record("tools/call")?;
                json!({"jsonrpc":"2.0","id":request.get("id"),"result":{"content":[]}})
            } else {
                super::stdio::response(&request)?
            };
            let mut output = output
                .lock()
                .map_err(|_| io::Error::other("notification fixture stdout poisoned"))?;
            if notification {
                record("notification")?;
                writeln!(
                    output,
                    "{}",
                    json!({
                        "jsonrpc":"2.0","method":"notifications/tools/list_changed",
                        "params":{"fixture":"one-upstream-broadcast"}
                    })
                )?;
            }
            writeln!(output, "{response}")?;
            output.flush()
        }));
    }
    for worker in workers {
        worker
            .join()
            .map_err(|_| io::Error::other("notification fixture worker failed"))??;
    }
    Ok(())
}
