use super::*;

#[tokio::test]
async fn pseudo_discovery_reuses_first_zero_ttl_modern_catalog() -> io::Result<()> {
    for (pseudo, output) in [("help", "json"), ("list_tools", "raw")] {
        let fixture = Fixture::new().await?;
        configure_synthetic(&fixture, &["hidden", "echo", "echo"], Some(&["echo"])).await?;
        let mut configuration: Value =
            serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
                .map_err(io::Error::other)?;
        configuration
            .pointer_mut("/mcpServers/fixture/env")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| io::Error::other("missing fixture environment"))?
            .insert("MCP_CLI_SYNTHETIC_MODERN_ONCE".to_owned(), json!("1"));
        tokio::fs::write(&fixture.config, configuration.to_string()).await?;
        let mut proxy = RpcProcess::fixture_proxy(fixture.spawn(&["proxy", "fixture"])?)?;
        let pong = proxy
            .exchange(json!({"jsonrpc":"2.0","id":0,"method":"ping","params":{}}))
            .await?;
        assert_eq!(pong.get("result"), Some(&json!({})));
        proxy.finish().await?;

        let selector = format!("fixture.{pseudo}");
        let report = parse_json(
            &fixture
                .success(&["call", &selector, "--output", output, "--no-oauth"])
                .await?,
        )?;
        assert_eq!(report.get("mode"), Some(&json!("server")));
        assert_eq!(report.get("status"), Some(&json!("ok")));
        assert_eq!(report.get("era"), Some(&json!("modern")));
        assert_eq!(report.get("protocolVersion"), Some(&json!("2026-07-28")));
        assert_eq!(
            report.get("instructions"),
            Some(&json!("First catalog only."))
        );
        let tools = report
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| io::Error::other("missing tools"))?;
        assert_eq!(tools.len(), 1);
        assert_eq!(report.pointer("/tools/0/name"), Some(&json!("echo")));
        assert!(report.pointer("/tools/0/inputSchema").is_some());
        assert!(report.pointer("/tools/0/options").is_some());
        assert_eq!(fixture.event_count("tools/list").await?, 1);
        assert_eq!(fixture.event_count("tools/call").await?, 0);
        fixture.finish().await?;
    }
    Ok(())
}

#[tokio::test]
async fn selection_diagnostics_do_not_interpret_tool_names_as_transport_errors() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    configure_synthetic(&fixture, &["echo"], None).await?;
    warm_synthetic(&fixture).await?;
    for name in ["oauth", "timeout", "forbidden"] {
        let selector = format!("fixture.{name}");
        let output = fixture
            .command(&["list", &selector, "--json", "--no-oauth"])
            .await?;
        assert!(!output.status.success(), "{output:?}");
        let report = parse_json(&output)?;
        assert_eq!(report.get("status"), Some(&json!("error")));
        assert_eq!(report.pointer("/issue/kind"), Some(&json!("error")));
        assert!(report.get("authCommand").is_none());
        assert_eq!(report.get("protocolVersion"), Some(&json!("2025-06-18")));
    }
    assert_eq!(fixture.event_count("tools/call").await?, 0);
    fixture.finish().await
}

#[tokio::test]
async fn normalized_catalog_keeps_first_schema_and_rejects_ambiguous_options_before_dispatch()
-> io::Result<()> {
    let fixture = Fixture::new().await?;
    configure_synthetic(&fixture, &["echo"], None).await?;
    let catalog = json!([{}, {"name":""}, {"name":42},
        {"name":"echo","description":"first metadata","inputSchema":{
            "$defs":{"label":{"type":"string"}},"properties":{"pageSize":{"$ref":"#/$defs/label"}}}},
        {"name":"echo","description":"duplicate metadata","inputSchema":{"properties":{"pageSize":{"type":"integer"}}}},
        {"name":"ambiguous","inputSchema":{"properties":{"apiKey":{"type":"string"},"api_key":{"type":"string"}}}},
        {"name":"Å_Name","description":"unicode metadata"}]);
    set_catalog(&fixture, &catalog).await?;
    warm_synthetic(&fixture).await?;
    let report = parse_json(&fixture.success(&["list", "fixture", "--json"]).await?)?;
    let tools = report
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("missing tools"))?;
    assert_eq!(tools.len(), 3);
    let echo = tools
        .iter()
        .find(|tool| tool.get("name") == Some(&json!("echo")))
        .ok_or_else(|| io::Error::other("echo missing"))?;
    assert_eq!(echo.get("description"), Some(&json!("first metadata")));
    assert_eq!(echo.pointer("/options/0/type"), Some(&json!("unknown")));
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name") == Some(&json!("Å_Name")))
    );
    let called = parse_json(
        &fixture
            .success(&[
                "call",
                "fixture.echo",
                "--page-size",
                "123",
                "--output",
                "json",
            ])
            .await?,
    )?;
    assert_eq!(called.pointer("/arguments/pageSize"), Some(&json!("123")));
    let rejected = fixture
        .command(&[
            "call",
            "fixture.ambiguous",
            "--api-key",
            "value",
            "--output",
            "json",
        ])
        .await?;
    assert!(!rejected.status.success(), "{rejected:?}");
    assert!(
        parse_json(&rejected)?
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|message| message.contains("ambiguous"))
    );
    assert_eq!(fixture.event_count("tools/call").await?, 1);
    let called = parse_json(
        &fixture
            .success(&[
                "call",
                "fixture.ambiguous",
                "--apiKey",
                "value",
                "--output",
                "json",
            ])
            .await?,
    )?;
    assert_eq!(called.pointer("/arguments/apiKey"), Some(&json!("value")));
    assert_eq!(fixture.event_count("tools/call").await?, 2);
    fixture.finish().await
}

async fn set_catalog(fixture: &Fixture, catalog: &Value) -> io::Result<()> {
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    configuration
        .pointer_mut("/mcpServers/fixture/env")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("missing fixture environment"))?
        .insert(
            "MCP_CLI_SYNTHETIC_CATALOG".to_owned(),
            json!(catalog.to_string()),
        );
    tokio::fs::write(&fixture.config, configuration.to_string()).await
}

#[tokio::test]
async fn discovery_drops_nonobject_catalog_entries() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    configure_synthetic(&fixture, &["echo"], None).await?;
    set_catalog(
        &fixture,
        &json!([null, false, 42, [], {"name":"echo","inputSchema":{"properties":{}}}]),
    )
    .await?;
    warm_synthetic(&fixture).await?;
    let report = parse_json(&fixture.success(&["list", "fixture", "--json"]).await?)?;
    assert_eq!(report.get("status"), Some(&json!("ok")), "{report}");
    let tools = report
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other(format!("missing tools: {report}")))?;
    assert_eq!(tools.len(), 1);
    assert_eq!(
        tools.first().and_then(|tool| tool.get("name")),
        Some(&json!("echo"))
    );
    fixture.finish().await
}
