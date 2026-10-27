use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use serde_json::{Value, json};

#[path = "mcp_cli_fixtures/http.rs"]
mod http;
#[path = "config_import_fixtures/support.rs"]
mod support;
use support::Fixture;

#[tokio::test]
async fn mcporter_default_import_merges_home_and_project_without_changing_sources() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join(".mcporter").join("mcporter.json");
    let project = fixture.project.join("config").join("mcporter.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{
            "local":{"command":"node --flag","env":{"TOKEN":"${UNSET_IMPORT_TOKEN}"},
                "lifecycle":{"mode":"keep-alive"}},
            "remote":{"baseUrl":"https://example.invalid/home","auth":"oauth",
                "clientName":"import-fixture","tokenCacheDir":"tokens"},
            "shared":{"command":"home-server"}
        },"imports":[]}),
    )?;
    fixture.write(
        &project,
        &json!({"mcpServers":{
            "shared":{"command":"project-server","cwd":"workers"}
        }}),
    )?;
    let original_home = std::fs::read(&source)?;
    let original_project = std::fs::read(&project)?;
    let preview = fixture
        .success(&["import", "mcporter", "--dry-run", "--json"])
        .await?;
    assert!(!fixture.home_config().exists());
    let preview: Value = serde_json::from_slice(&preview.stdout)?;
    assert_eq!(
        preview
            .get("servers")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(3)
    );
    fixture.success(&["import", "mcporter"]).await?;
    fixture.success(&["config", "import", "mcporter"]).await?;
    let document: Value = serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
    assert_eq!(
        document.pointer("/mcpServers/local/command"),
        Some(&json!(["node", "--flag"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/local/env/TOKEN"),
        Some(&json!("${UNSET_IMPORT_TOKEN}"))
    );
    assert_eq!(
        document.pointer("/mcpServers/local/lifecycle"),
        Some(&json!({"mode":"keep-alive"}))
    );
    assert_eq!(
        document.pointer("/mcpServers/remote/baseUrl"),
        Some(&json!("https://example.invalid/home"))
    );
    assert_eq!(
        document.pointer("/mcpServers/remote/tokenCacheDir"),
        Some(&json!(
            source.parent().context("source parent")?.join("tokens")
        ))
    );
    assert_eq!(
        document.pointer("/mcpServers/shared/command"),
        Some(&json!(["project-server"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/shared/cwd"),
        Some(&json!(
            project.parent().context("project parent")?.join("workers")
        ))
    );
    assert_eq!(std::fs::read(source)?, original_home);
    assert_eq!(std::fs::read(project)?, original_project);
    fixture.finish().await
}

#[tokio::test]
async fn imported_mcporter_server_keeps_its_endpoint_headers_and_shared_session() -> Result<()> {
    let fixture = Fixture::new()?;
    let remote = http::HttpFixture::new().await?;
    let source = fixture.directory.join("selected-mcporter.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{"same-server":{
            "baseUrl":remote.url,
            "headers":{"X-Import-Fixture":"${IMPORT_FIXTURE_HEADER}"},
            "env":{"IMPORT_FIXTURE_HEADER":"same-value"},
            "lifecycle":{"mode":"keep-alive"}
        }},"imports":[]}),
    )?;
    fixture
        .success(&[
            "import",
            "mcporter",
            "--from",
            source.to_str().context("source path")?,
        ])
        .await?;
    fixture
        .success(&["list", "same-server", "--json", "--no-oauth"])
        .await?;
    for _ in 0..2 {
        let output = fixture
            .success(&[
                "call",
                "same-server.headers",
                "--output",
                "json",
                "--no-oauth",
            ])
            .await?;
        let result: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            result.pointer("/headers/x-import-fixture"),
            Some(&json!("same-value"))
        );
        assert_eq!(
            result.pointer("/headers/mcp-session-id"),
            Some(&json!("mock-session"))
        );
    }
    assert_eq!(remote.initializations.load(Ordering::SeqCst), 1);
    assert!(!remote.headers.lock().await.is_empty());
    fixture.finish().await
}

#[tokio::test]
async fn imported_unavailable_server_reports_failure_with_opt_in_health_exit_codes() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("mcporter.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{"offline":{
            "command":"mcp-pool-intentionally-absent-import-fixture"
        }}}),
    )?;
    fixture
        .success(&[
            "import",
            "mcporter",
            "--from",
            source.to_str().context("source path")?,
        ])
        .await?;
    for command in ["list", "describe", "list-tools"] {
        let output = fixture
            .success(&[command, "offline", "--json", "--no-oauth"])
            .await?;
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report.get("status"), Some(&json!("offline")));
        assert!(
            report
                .get("error")
                .and_then(Value::as_str)
                .is_some_and(|error| !error.is_empty())
        );
    }
    let strict = fixture
        .run(&["list", "offline", "--json", "--no-oauth", "--exit-code"])
        .await?;
    assert!(!strict.status.success());
    let strict_report: Value = serde_json::from_slice(&strict.stdout)?;
    assert_eq!(strict_report.get("status"), Some(&json!("offline")));
    let quiet = fixture
        .run(&["list", "offline", "--quiet", "--no-oauth"])
        .await?;
    assert!(!quiet.status.success());
    assert!(quiet.stdout.is_empty());
    assert!(quiet.stderr.is_empty());
    fixture.write(
        &fixture.project.join(".mcp").join("config.json"),
        &json!({"mcpServers":{},"imports":["cursor"]}),
    )?;
    let warned = fixture.success(&["list", "offline", "--no-oauth"]).await?;
    assert!(String::from_utf8_lossy(&warned.stderr).contains("legacy import"));
    for arguments in [
        ["list", "offline", "--quiet"],
        ["list", "not-configured", "--quiet"],
        ["list", "--not-a-discovery-flag", "--quiet"],
    ] {
        let quiet = fixture.run(&arguments).await?;
        assert!(!quiet.status.success(), "{arguments:?}");
        assert!(quiet.stdout.is_empty(), "{arguments:?}");
        assert!(quiet.stderr.is_empty(), "{arguments:?} {quiet:?}");
    }
    let unknown = fixture.run(&["list", "not-configured", "--json"]).await?;
    assert!(!unknown.status.success());
    fixture.finish().await
}
