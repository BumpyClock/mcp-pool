use std::io;

use serde_json::{Value, json};

#[path = "mcp_cli_fixtures/stdio.rs"]
mod stdio;
#[path = "mcp_cli_fixtures/support.rs"]
#[allow(dead_code)]
mod support;

use support::{Fixture, RpcProcess, initialize, parse_json};

#[tokio::test]
async fn missing_tool_discovery_fails_and_quiet_health_checks_remain_silent() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    for command in ["list", "describe", "list-tools"] {
        let output = fixture
            .command(&[command, "fixture.unknown_tool", "--json", "--no-oauth"])
            .await?;
        assert!(!output.status.success(), "{output:?}");
        let report = parse_json(&output)?;
        assert_eq!(report.get("status"), Some(&json!("error")));
        assert!(
            report
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| error.contains("unknown_tool"))
        );
    }
    let quiet = fixture
        .command(&["list", "fixture.unknown_tool", "--quiet", "--no-oauth"])
        .await?;
    assert!(!quiet.status.success());
    assert!(quiet.stdout.is_empty());
    assert!(quiet.stderr.is_empty());
    assert_eq!(fixture.event_count("tools/call").await?, 0);
    fixture.finish().await
}

async fn configure_synthetic(
    fixture: &Fixture,
    names: &[&str],
    allowed: Option<&[&str]>,
) -> io::Result<()> {
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    let server = configuration
        .pointer_mut("/mcpServers/fixture")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("missing fixture server"))?;
    let executable = std::env::current_exe()?;
    server.insert(
        "command".to_owned(),
        json!([
            executable.to_string_lossy(),
            "--exact",
            "synthetic::upstream",
            "--nocapture",
            "--quiet"
        ]),
    );
    server.remove("args");
    let environment = server
        .get_mut("env")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("missing fixture environment"))?;
    environment.insert("MCP_POOL_SYNTHETIC".to_owned(), json!("1"));
    environment.insert(
        "MCP_POOL_SYNTHETIC_COUNTER".to_owned(),
        json!(fixture.counter.to_string_lossy()),
    );
    environment.insert(
        "MCP_POOL_SYNTHETIC_TOOLS".to_owned(),
        json!(names.join(",")),
    );
    server.remove("allowedTools");
    server.remove("allowed_tools");
    server.remove("blockedTools");
    server.remove("blocked_tools");
    if let Some(allowed) = allowed {
        server.insert("allowedTools".to_owned(), json!(allowed));
    }
    tokio::fs::write(&fixture.config, configuration.to_string()).await
}

async fn warm_synthetic(fixture: &Fixture) -> io::Result<()> {
    let mut proxy = RpcProcess::fixture_proxy(fixture.spawn(&["proxy", "fixture"])?)?;
    let response = proxy.exchange(initialize(1)).await?;
    assert_eq!(
        response.pointer("/result/serverInfo/name"),
        Some(&json!("synthetic"))
    );
    proxy.finish().await
}

#[tokio::test]
async fn exact_and_normalized_names_call_the_real_upstream_tool() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    configuration
        .pointer_mut("/mcpServers/fixture/env")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("missing fixture environment"))?
        .insert("MCP_POOL_TEST_DOTTED_TOOL".to_owned(), json!("1"));
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    fixture.warm("fixture").await?;

    let exact = parse_json(
        &fixture
            .success(&["call", "fixture.echo.v2", "label=exact", "--output", "json"])
            .await?,
    )?;
    assert_eq!(exact.pointer("/arguments/label"), Some(&json!("exact")));
    let normalized = parse_json(
        &fixture
            .success(&[
                "call",
                "fixture.Echo-V2",
                "label=normalized",
                "--output",
                "json",
            ])
            .await?,
    )?;
    assert_eq!(
        normalized.pointer("/arguments/label"),
        Some(&json!("normalized"))
    );
    assert_eq!(fixture.event_count("tools/call").await?, 2);
    fixture.finish().await
}

#[tokio::test]
async fn missing_pseudo_tools_fall_back_to_discovery_without_calling_a_tool() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    configure_synthetic(&fixture, &["echo"], None).await?;
    warm_synthetic(&fixture).await?;
    let before = fixture.event_count("tools/call").await?;

    for pseudo in ["list_tools", "help"] {
        let selector = format!("fixture.{pseudo}");
        let output = fixture
            .success(&["call", selector.as_str(), "--output", "json"])
            .await?;
        let discovery = parse_json(&output)?;
        assert_eq!(discovery.get("mode"), Some(&json!("server")));
        assert_eq!(discovery.pointer("/tools/0/name"), Some(&json!("echo")));
    }
    let raw = parse_json(
        &fixture
            .success(&[
                "call",
                "fixture.help",
                "--output",
                "raw",
                "--timeout",
                "5000",
                "--no-oauth",
            ])
            .await?,
    )?;
    assert_eq!(raw.get("mode"), Some(&json!("server")));
    assert_eq!(raw.pointer("/tools/0/name"), Some(&json!("echo")));
    let text = fixture
        .success(&["call", "fixture.help", "--output", "text", "--no-oauth"])
        .await?;
    assert!(String::from_utf8_lossy(&text.stdout).contains("echo"));
    assert_eq!(fixture.event_count("tools/call").await?, before);
    fixture.finish().await
}

#[tokio::test]
async fn pseudo_discovery_rejects_tool_arguments_without_calling_a_tool() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    configure_synthetic(&fixture, &["echo"], None).await?;
    warm_synthetic(&fixture).await?;
    let before = fixture.event_count("tools/call").await?;

    let rejected = fixture
        .command(&["call", "fixture.help", "label=ignored", "--output", "json"])
        .await?;
    assert!(!rejected.status.success(), "{rejected:?}");
    let rejected_output = format!(
        "{}{}",
        String::from_utf8_lossy(&rejected.stdout),
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(rejected_output.contains("does not accept"));
    assert_eq!(fixture.event_count("tools/call").await?, before);

    let empty_arguments = fixture
        .command(&[
            "call",
            "fixture.list_tools",
            "--args",
            "{}",
            "--output",
            "json",
        ])
        .await?;
    assert!(!empty_arguments.status.success(), "{empty_arguments:?}");
    assert!(
        format!(
            "{}{}",
            String::from_utf8_lossy(&empty_arguments.stdout),
            String::from_utf8_lossy(&empty_arguments.stderr)
        )
        .contains("does not accept")
    );
    assert_eq!(fixture.event_count("tools/call").await?, before);
    fixture.finish().await
}

#[tokio::test]
async fn real_filtered_pseudo_tools_block_fallbacks_and_calls() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    configure_synthetic(&fixture, &["list_tools", "help", "echo"], Some(&["echo"])).await?;
    warm_synthetic(&fixture).await?;
    let before = fixture.event_count("tools/call").await?;

    for selector in ["fixture.list_tools", "fixture.list-tools", "fixture.help"] {
        let rejected = fixture
            .command(&["call", selector, "--output", "json"])
            .await?;
        assert!(!rejected.status.success(), "{selector}: {rejected:?}");
        let rejected_output = format!(
            "{}{}",
            String::from_utf8_lossy(&rejected.stdout),
            String::from_utf8_lossy(&rejected.stderr)
        );
        assert!(
            rejected_output.contains("excluded"),
            "{selector}: {rejected:?}"
        );
    }
    assert_eq!(fixture.event_count("tools/call").await?, before);
    fixture.finish().await
}

#[tokio::test]
async fn ambiguous_and_typo_names_do_not_call_the_upstream() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    let before = fixture.event_count("tools/call").await?;

    let typo = fixture
        .command(&["call", "fixture.ech", "label=typo"])
        .await?;
    assert!(!typo.status.success(), "{typo:?}");
    assert!(String::from_utf8_lossy(&typo.stderr).contains("Did you mean"));
    assert_eq!(fixture.event_count("tools/call").await?, before);
    fixture.finish().await
}

#[tokio::test]
async fn filtered_tools_cannot_be_called_or_suggested() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    let server = configuration
        .pointer_mut("/mcpServers/fixture")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("missing fixture server"))?;
    server.insert("allowedTools".to_owned(), json!(["echo"]));
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    fixture.warm("fixture").await?;
    let before = fixture.event_count("tools/call").await?;

    let hidden_typo = fixture
        .command(&["call", "fixture.delaye", "label=hidden-typo"])
        .await?;
    assert!(!hidden_typo.status.success(), "{hidden_typo:?}");
    let typo_error = String::from_utf8_lossy(&hidden_typo.stderr);
    assert!(typo_error.contains("Unknown tool"));
    assert!(!typo_error.contains("delayed"));
    assert_eq!(fixture.event_count("tools/call").await?, before);

    let excluded = fixture
        .command(&["call", "fixture.delayed", "label=excluded"])
        .await?;
    assert!(!excluded.status.success(), "{excluded:?}");
    assert!(String::from_utf8_lossy(&excluded.stderr).contains("excluded"));
    assert_eq!(fixture.event_count("tools/call").await?, before);
    fixture.finish().await
}

mod synthetic {
    use std::fs::OpenOptions;
    use std::io::{self, BufRead, Write};

    use serde_json::{Value, json};

    fn record(event: &str) -> io::Result<()> {
        let path = std::env::var_os("MCP_POOL_SYNTHETIC_COUNTER")
            .ok_or_else(|| io::Error::other("synthetic counter path is missing"))?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?
            .write_all(format!("{event}\n").as_bytes())
    }

    fn tool(name: &str) -> Value {
        json!({
            "name":name,
            "description":format!("Synthetic {name}"),
            "inputSchema":{"type":"object","properties":{
                "label":{"type":"string"}
            }}
        })
    }

    fn response(request: &Value, names: &[&str]) -> io::Result<Value> {
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let id = request.get("id").cloned();
        let result = match method {
            "initialize" => {
                record("initialize")?;
                return Ok(json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "result":{
                        "protocolVersion":"2025-06-18",
                        "capabilities":{"tools":{}},
                        "serverInfo":{"name":"synthetic","version":"1"}
                    }
                }));
            }
            "server/discover" => {
                return Ok(json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "error":{"code":-32601,"message":"method not found"}
                }));
            }
            "tools/list" => {
                record("tools/list")?;
                json!({"tools":names.iter().map(|name| tool(name)).collect::<Vec<_>>()})
            }
            "tools/call" => {
                record("tools/call")?;
                json!({
                    "content":[{"type":"text","text":"synthetic call"}],
                    "structuredContent":{"called":request.pointer("/params/name")}
                })
            }
            _ => {
                return Ok(json!({
                    "jsonrpc":"2.0",
                    "id":id,
                    "error":{"code":-32601,"message":"method not found"}
                }));
            }
        };
        Ok(json!({"jsonrpc":"2.0","id":id,"result":result}))
    }

    #[test]
    fn upstream() -> io::Result<()> {
        if std::env::var_os("MCP_POOL_SYNTHETIC").is_none() {
            return Ok(());
        }
        let configured_names =
            std::env::var("MCP_POOL_SYNTHETIC_TOOLS").map_err(io::Error::other)?;
        let names = configured_names
            .split(',')
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        let stdout = io::stdout();
        let mut output = stdout.lock();
        for line in io::stdin().lock().lines() {
            let request: Value = serde_json::from_str(&line?).map_err(io::Error::other)?;
            if request.get("id").is_none() {
                continue;
            }
            writeln!(output, "{}", response(&request, &names)?)?;
            output.flush()?;
        }
        Ok(())
    }
}
