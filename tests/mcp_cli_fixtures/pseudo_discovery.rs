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
            .insert("MCP_POOL_SYNTHETIC_MODERN_ONCE".to_owned(), json!("1"));
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
