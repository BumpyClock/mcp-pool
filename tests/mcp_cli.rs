use std::io;

use serde_json::{Value, json};

#[path = "mcp_cli_fixtures/admin.rs"]
mod admin;
#[path = "mcp_cli_fixtures/bridge_notifications.rs"]
mod bridge_notifications;
#[path = "mcp_cli_fixtures/http.rs"]
mod http;
#[path = "mcp_cli_fixtures/identity.rs"]
mod identity;
#[path = "mcp_cli_fixtures/stdio.rs"]
mod stdio;
#[path = "mcp_cli_fixtures/support.rs"]
mod support;

use support::{Fixture, RpcProcess, initialize, parse_json};

#[tokio::test]
async fn active_discovery_paginates_and_retains_schema() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    let listed = parse_json(&fixture.success(&["list", "fixture", "--json"]).await?)?;
    assert_eq!(listed.get("mode"), Some(&json!("server")));
    assert_eq!(listed.get("name"), Some(&json!("fixture")));
    assert_eq!(listed.get("status"), Some(&json!("ok")));
    let tools = listed
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("list omitted tools"))?;
    let names: Vec<_> = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    assert_eq!(names, ["delayed", "echo", "fail_rpc", "fail_tool"]);
    assert_eq!(
        listed.pointer("/tools/1/inputSchema/required"),
        Some(&json!(["label"]))
    );
    assert!(fixture.event_count("tools/list").await? >= 2);
    let schema = fixture.success(&["list", "fixture", "--schema"]).await?;
    assert!(String::from_utf8_lossy(&schema.stdout).contains("label"));
    let quiet = fixture.success(&["list", "fixture", "--quiet"]).await?;
    assert!(quiet.stdout.is_empty(), "{quiet:?}");
    fixture.finish().await
}

#[tokio::test]
async fn two_cli_calls_and_compatibility_proxy_share_one_upstream() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let mut proxy = RpcProcess::fixture_proxy(fixture.spawn(&["proxy", "fixture"])?)?;
    let initialized = proxy.exchange(initialize(70)).await?;
    assert_eq!(
        initialized.pointer("/result/serverInfo/name"),
        Some(&json!("controlled-fixture"))
    );

    let first_arguments = [
        "call",
        "fixture.delayed",
        "label=first",
        "delay_ms=160",
        "--output",
        "json",
    ];
    let second_arguments = [
        "call",
        "fixture.echo",
        "label=second",
        "count=7",
        "enabled=true",
        "--output",
        "json",
    ];
    let proxy_request = json!({"jsonrpc":"2.0","id":71,"method":"tools/call","params":{
        "name":"echo","arguments":{"label":"proxy","count":11}
    }});
    let (first, second, third) = tokio::try_join!(
        fixture.success(&first_arguments),
        fixture.success(&second_arguments),
        proxy.exchange(proxy_request)
    )?;
    assert_eq!(
        parse_json(&first)?.pointer("/arguments/label"),
        Some(&json!("first"))
    );
    let second = parse_json(&second)?;
    assert_eq!(
        second.get("arguments"),
        Some(&json!({"label":"second","count":7,"enabled":true}))
    );
    assert_eq!(
        second.get("environment"),
        Some(&json!("synthetic environment"))
    );
    assert_eq!(
        second.get("cwd"),
        Some(&json!(fixture.working_directory.to_string_lossy()))
    );
    assert_eq!(third.get("id"), Some(&json!(71)));
    assert_eq!(
        third.pointer("/result/structuredContent/arguments"),
        Some(&json!({"label":"proxy","count":11}))
    );
    let repeated = fixture
        .success(&["call", "fixture.echo", "label=again", "--output", "json"])
        .await?;
    assert_eq!(
        parse_json(&repeated)?.pointer("/arguments/label"),
        Some(&json!("again"))
    );
    assert_eq!(fixture.event_count("spawn").await?, 1);
    assert_eq!(fixture.event_count("initialize").await?, 1);
    assert_eq!(fixture.event_count("tools/call").await?, 4);
    proxy.finish().await?;
    fixture.finish().await
}

#[tokio::test]
async fn call_forms_and_output_modes_preserve_values() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    let forms: &[&[&str]] = &[
        &[
            "call",
            "fixture",
            "echo",
            "label:separate",
            "count:3",
            "--output",
            "json",
        ],
        &[
            "call",
            "fixture.echo(label: \"function\", count: 4)",
            "--output",
            "json",
        ],
        &[
            "call",
            "fixture.echo",
            "--args",
            "{\"label\":\"json\",\"count\":5}",
            "--output",
            "json",
        ],
        &[
            "call", "--server", "fixture", "--tool", "echo", "--label", "named", "--count", "6",
            "--output", "json",
        ],
        &["call", "fixture.echo", "positional", "--output", "json"],
    ];
    for (arguments, expected) in forms.iter().zip([
        json!({"label":"separate","count":3}),
        json!({"label":"function","count":4}),
        json!({"label":"json","count":5}),
        json!({"label":"named","count":6}),
        json!({"label":"positional"}),
    ]) {
        assert_eq!(
            parse_json(&fixture.success(arguments).await?)?.get("arguments"),
            Some(&expected)
        );
    }
    let stdin = fixture
        .command_input(
            &["call", "fixture.echo", "--args", "-", "--output", "json"],
            Some("{\"label\":\"stdin\",\"enabled\":false}\n"),
        )
        .await?;
    assert!(stdin.status.success(), "{stdin:?}");
    assert_eq!(
        parse_json(&stdin)?.get("arguments"),
        Some(&json!({"label":"stdin","enabled":false}))
    );
    let raw = parse_json(
        &fixture
            .success(&["call", "fixture.echo", "label=raw", "--output", "raw"])
            .await?,
    )?;
    assert_eq!(raw.pointer("/content/0/type"), Some(&json!("text")));
    assert_eq!(
        raw.pointer("/structuredContent/arguments/label"),
        Some(&json!("raw"))
    );
    let text = fixture
        .success(&["call", "fixture.echo", "label=text", "--output", "text"])
        .await?;
    assert!(String::from_utf8_lossy(&text.stdout).contains("\"label\":\"text\""));
    fixture.warm("array").await?;
    let array = fixture
        .success(&["call", "array.echo", "label=array", "--output", "json"])
        .await?;
    assert_eq!(
        parse_json(&array)?.pointer("/arguments/label"),
        Some(&json!("array"))
    );
    fixture.finish().await
}

#[tokio::test]
async fn rpc_tool_and_deadline_errors_exit_unsuccessfully_without_replay() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    for (arguments, message) in [
        (
            vec!["call", "fixture.fail_rpc", "label=rpc"],
            "fixture RPC failure",
        ),
        (
            vec!["call", "fixture.fail_tool", "label=tool"],
            "fixture tool failure",
        ),
    ] {
        let output = fixture.command(&arguments).await?;
        assert!(!output.status.success(), "{output:?}");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(combined.contains(message), "{combined}");
    }
    let timeout = fixture
        .command(&[
            "call",
            "fixture.delayed",
            "label=late",
            "delay_ms=400",
            "--timeout",
            "50",
        ])
        .await?;
    assert!(!timeout.status.success(), "{timeout:?}");
    assert!(
        String::from_utf8_lossy(&timeout.stderr).contains("deadline"),
        "{timeout:?}"
    );
    let after = fixture
        .success(&["call", "fixture.echo", "label=after", "--output", "json"])
        .await?;
    assert_eq!(
        parse_json(&after)?.pointer("/arguments/label"),
        Some(&json!("after"))
    );
    assert_eq!(fixture.event_count("tools/call").await?, 4);
    assert_eq!(fixture.event_count("spawn").await?, 1);
    fixture.finish().await
}

#[tokio::test]
async fn resource_list_pages_and_reads_preserve_uri() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    let listing = parse_json(&fixture.success(&["resources", "fixture", "--json"]).await?)?;
    assert_eq!(
        listing,
        json!({"resources":[
            {"uri":"fixture://first","name":"first"}, {"uri":"fixture://second","name":"second"}
        ]})
    );
    let resource = parse_json(
        &fixture
            .success(&["resource", "fixture", "fixture://second", "--output", "raw"])
            .await?,
    )?;
    assert_eq!(
        resource,
        json!({"contents":[{
            "uri":"fixture://second","mimeType":"text/plain","text":"fixture resource body"
        }]})
    );
    fixture.finish().await
}

#[tokio::test]
async fn bridge_aggregates_tools_and_reuses_cli_upstream() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    fixture
        .success(&["call", "fixture.echo", "label=before"])
        .await?;
    let mut bridge =
        RpcProcess::new(fixture.spawn(&["serve", "--stdio", "--servers", "fixture"])?)?;
    let initialization = bridge.exchange(initialize(90)).await?;
    assert!(initialization.get("result").is_some(), "{initialization}");
    let listing = bridge
        .exchange(json!({"jsonrpc":"2.0","id":"tools","method":"tools/list","params":{}}))
        .await?;
    let tools = listing
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("bridge omitted tools"))?;
    let mut names: Vec<_> = tools
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "fixture__delayed",
            "fixture__echo",
            "fixture__fail_rpc",
            "fixture__fail_tool"
        ]
    );
    let response = bridge
        .exchange(
            json!({"jsonrpc":"2.0","id":"call","method":"tools/call","params":{
                "name":"fixture__echo","arguments":{"label":"bridge"}
            }}),
        )
        .await?;
    assert_eq!(response.get("id"), Some(&json!("call")));
    assert_eq!(
        response.pointer("/result/structuredContent/arguments/label"),
        Some(&json!("bridge"))
    );
    assert_eq!(fixture.event_count("spawn").await?, 1);
    assert_eq!(fixture.event_count("initialize").await?, 1);
    bridge.finish().await?;
    fixture.finish().await
}

#[tokio::test]
async fn base_url_headers_and_http_session_are_reused() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let remote = http::HttpFixture::new().await?;
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    let servers = configuration
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("fixture config omitted mcpServers"))?;
    servers.insert(
        "remote".into(),
        json!({
            "baseUrl":remote.url, "headers":{"X-Fixture":"synthetic-header"},
            "env":{"HOME":fixture.home,"USERPROFILE":fixture.home,"XDG_DATA_HOME":fixture.home}
        }),
    );
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    for _ in 0..2 {
        let response = parse_json(
            &fixture
                .success(&["call", "remote.headers", "--output", "json"])
                .await?,
        )?;
        assert_eq!(
            response.pointer("/headers/x-fixture"),
            Some(&json!("synthetic-header"))
        );
        assert_eq!(
            response.pointer("/headers/mcp-session-id"),
            Some(&json!("mock-session"))
        );
        assert_eq!(
            response.pointer("/headers/mcp-protocol-version"),
            Some(&json!("2025-06-18"))
        );
    }
    assert_eq!(
        remote
            .initializations
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert!(remote.headers.lock().await.iter().all(|headers|
        headers.get("x-fixture").map(String::as_str) == Some("synthetic-header")
    ));
    fixture.finish().await
}

#[tokio::test]
async fn config_administration_is_local_and_invalid_input_does_not_launch_upstream()
-> io::Result<()> {
    let fixture = Fixture::new().await?;
    let listing = fixture.success(&["config", "list", "--json"]).await?;
    let listing = parse_json(&listing)?;
    assert!(listing.to_string().contains("fixture"));
    let server = parse_json(
        &fixture
            .success(&["config", "get", "fixture", "--json"])
            .await?,
    )?;
    assert_eq!(server.get("name"), Some(&json!("fixture")));
    assert_eq!(server.get("transport"), Some(&json!("stdio")));
    assert_eq!(server.get("cwd"), Some(&json!(fixture.working_directory)));
    assert_eq!(
        server.pointer("/env/MCP_POOL_TEST_VALUE"),
        Some(&json!("[redacted]"))
    );
    assert!(!server.to_string().contains("synthetic environment"));
    assert!(!fixture.counter.exists());
    fixture
        .success(&["config", "add", "added", "--url", "http://127.0.0.1:1/mcp"])
        .await?;
    let added = parse_json(
        &fixture
            .success(&["config", "get", "added", "--json"])
            .await?,
    )?;
    assert!(added.to_string().contains("http://127.0.0.1:1/mcp"));
    fixture.success(&["config", "remove", "added"]).await?;
    let missing = fixture
        .command(&["config", "get", "added", "--json"])
        .await?;
    assert!(!missing.status.success());
    assert!(!fixture.counter.exists());
    tokio::fs::write(
        &fixture.config,
        "{\"mcpServers\":{\"broken\":{\"command\":17}}}",
    )
    .await?;
    let invalid = fixture.command(&["list", "--json"]).await?;
    assert!(!invalid.status.success(), "{invalid:?}");
    assert!(!fixture.counter.exists());
    fixture.finish().await
}

#[tokio::test]
async fn deferred_imports_do_not_read_other_configuration() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    let imported = fixture.home.join("must-not-import.json");
    tokio::fs::write(
        &imported,
        "{\"mcpServers\":{\"forbidden\":{\"command\":\"missing\"}}}",
    )
    .await?;
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    let object = configuration
        .as_object_mut()
        .ok_or_else(|| io::Error::other("config is not object"))?;
    object.insert("imports".into(), json!([imported]));
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    let output = fixture.success(&["list", "fixture", "--json"]).await?;
    assert_eq!(parse_json(&output)?.get("name"), Some(&json!("fixture")));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("forbidden"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("deferred"));
    fixture.finish().await
}
