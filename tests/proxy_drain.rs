use std::io;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

#[path = "common/fixture_lifecycle.rs"]
mod fixture_lifecycle;
#[path = "mcp_cli_fixtures/stdio.rs"]
mod stdio;
#[path = "mcp_cli_fixtures/support.rs"]
#[allow(dead_code)]
mod support;

#[tokio::test]
async fn stdin_eof_keeps_pending_pool_responses_until_the_drain_completes() -> io::Result<()> {
    for arguments in [&["proxy", "fixture"][..], &["pool", "proxy", "fixture"][..]] {
        let fixture = support::Fixture::new().await?;
        let gate = fixture.home.join("release-list");
        let mut configuration: Value =
            serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
                .map_err(io::Error::other)?;
        configuration
            .pointer_mut("/mcpServers/fixture/env")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| io::Error::other("fixture environment missing"))?
            .insert(stdio::LIST_GATE.to_owned(), json!(gate));
        tokio::fs::write(&fixture.config, configuration.to_string()).await?;
        fixture.warm("fixture").await?;
        let mut proxy = fixture.spawn_environment(arguments, &[("MCP_CLI_DEBUG", "1")])?;
        let mut input = proxy
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("proxy stdin missing"))?;
        input
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":37,\"method\":\"tools/list\",\"params\":{}}\n")
            .await?;
        input.flush().await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while fixture.event_count("tools/list").await? == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            Ok::<(), io::Error>(())
        })
        .await
        .map_err(io::Error::other)??;
        input.shutdown().await?;
        drop(input);
        let log = fixture.home.join("state/logs/mcp-cli.log");
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match tokio::fs::read_to_string(&log).await {
                    Ok(contents) if contents.contains("proxy_eof dir=stdin->socket") => {
                        return Ok::<(), io::Error>(());
                    }
                    Ok(_) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .map_err(io::Error::other)??;
        tokio::fs::write(&gate, b"release").await?;
        let output = tokio::time::timeout(Duration::from_secs(5), proxy.wait_with_output())
            .await
            .map_err(io::Error::other)??;
        assert!(output.status.success(), "{output:?}");
        let response: Value = serde_json::from_slice(&output.stdout).map_err(|error| {
            io::Error::other(format!(
                "pending response lost after stdin EOF: {error}; {output:?}"
            ))
        })?;
        assert_eq!(response.get("id"), Some(&json!(37)));
        assert_eq!(
            response.pointer("/result/tools/0/name"),
            Some(&json!("echo"))
        );
        assert_eq!(fixture.event_count("tools/list").await?, 1);
        fixture.finish().await?;
    }
    Ok(())
}
