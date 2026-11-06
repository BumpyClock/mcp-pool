use anyhow::Context;
use std::io;
use std::path::Path;
use std::time::Duration;

#[path = "common/fixture_lifecycle.rs"]
mod fixture_lifecycle;
#[allow(dead_code)]
#[path = "config_import_fixtures/support.rs"]
mod import_support;
#[allow(dead_code)]
#[path = "mcp_cli_fixtures/support.rs"]
mod mcp_support;
#[path = "mcp_cli_fixtures/stdio.rs"]
mod stdio;
use fixture_lifecycle::{FixtureLifecycle, fixture_environment, fixture_home};

const BINARY: &str = env!("CARGO_BIN_EXE_mcp-cli");
const CHILD: &str = "FIXTURE_ISOLATION_CHILD";
const UNRELATED: &str = "FIXTURE_UNRELATED_VALUE";

#[tokio::test]
async fn poisoned_host_settings_do_not_redirect_fixtures() -> anyhow::Result<()> {
    let sentinel = fixture_home::create("fixture-sentinel")?;
    let config = sentinel.join("config.json");
    let credentials = sentinel.join("credentials.json");
    let contents = br#"{"mcpServers":{}}"#;
    std::fs::write(&config, contents)?;
    std::fs::write(&credentials, b"external credentials must survive")?;
    let mut external = FixtureLifecycle::new(
        Path::new(BINARY),
        &sentinel,
        &sentinel,
        &sentinel,
        Some(&config),
    );
    let mut start = external.command();
    start.args(["pool", "serve"]);
    external.start(start)?;
    external.ready(Duration::from_secs(10)).await?;
    let mut child = tokio::process::Command::new(std::env::current_exe()?);
    child
        .args(["--exact", "poisoned_host_child", "--nocapture"])
        .env(CHILD, "1")
        .env(UNRELATED, "preserved host value")
        .kill_on_drop(true);
    for prefix in ["MCP_CLI", "MCP_POOL"] {
        child
            .env(format!("{prefix}_HOME"), &sentinel)
            .env(format!("{prefix}_CONFIG"), &config)
            .env(format!("{prefix}_CREDENTIALS_READ_ONLY"), "1")
            .env(format!("{prefix}_DEBUG"), "1")
            .env(format!("{prefix}_RAW_UPSTREAM_STDERR"), "1")
            .env(format!("{prefix}_NO_PROGRESS"), "1")
            .env(format!("{prefix}_UNRECOGNIZED_SETTING"), "also cleared");
    }
    let output = tokio::time::timeout(Duration::from_secs(60), child.output()).await??;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(std::fs::read(&config)?, contents);
    assert_eq!(
        std::fs::read(&credentials)?,
        b"external credentials must survive"
    );
    let status = tokio::process::Command::from(external.command())
        .args(["daemon", "status", "--json"])
        .output()
        .await?;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&status.stdout)?.get("running"),
        Some(&serde_json::json!(true))
    );
    external.finish().await?;
    Ok(())
}

#[tokio::test]
async fn poisoned_host_child() -> anyhow::Result<()> {
    if std::env::var_os(CHILD).is_none() {
        return Ok(());
    }
    let fixture = import_support::Fixture::new()?;
    let source = fixture.project.join(".mcp.json");
    fixture.write(
        &source,
        &serde_json::json!({"mcpServers":{"imported":{"command":"fixture-command"}}}),
    )?;
    fixture
        .success(&["import", "project", "--scope", "home"])
        .await?;
    let imported: serde_json::Value =
        serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
    assert!(imported.pointer("/mcpServers/imported").is_some());
    fixture.write(
        &fixture.home_config(),
        &serde_json::json!({"mcpServers": {
            "protected": {"url": "https://fixture.invalid/mcp", "auth": "oauth"}
        }}),
    )?;
    let tokens = fixture.directory.join("tokens.json");
    fixture.write(
        &tokens,
        &serde_json::json!({
            "access_token": "fixture-token", "token_type": "Bearer", "expires_at": 4102444800u64
        }),
    )?;
    fixture
        .success(&[
            "vault",
            "set",
            "protected",
            "--tokens-file",
            tokens.to_str().context("token path")?,
        ])
        .await?;
    assert!(
        fixture
            .home_config()
            .with_file_name("credentials.json")
            .exists()
    );
    fixture.finish().await?;

    let fixture = mcp_support::Fixture::new().await?;
    fixture.success(&["list", "fixture", "--json"]).await?;
    fixture.finish().await?;

    let mut probe = std::process::Command::new(std::env::current_exe()?);
    fixture_environment::isolate(&mut probe);
    probe
        .args(["--exact", "isolated_environment_probe", "--nocapture"])
        .env("MCP_POOL_DEBUG", "scenario override")
        .env("MCP_CLI_TEST_STDIO", "scenario fixture marker");
    let output = tokio::process::Command::from(probe).output().await?;
    assert!(output.status.success(), "{output:?}");
    Ok(())
}

#[test]
fn isolated_environment_probe() -> io::Result<()> {
    if std::env::var_os(CHILD).is_none() {
        return Ok(());
    }
    assert_eq!(
        std::env::var(UNRELATED).map_err(io::Error::other)?,
        "preserved host value"
    );
    assert!(std::env::var_os("PATH").is_some());
    let mut settings: Vec<_> = std::env::vars_os()
        .filter(|(name, _)| {
            let name = name.to_string_lossy().to_ascii_uppercase();
            name.starts_with("MCP_CLI_") || name.starts_with("MCP_POOL_")
        })
        .collect();
    settings.sort();
    assert_eq!(
        settings,
        vec![
            (
                "MCP_CLI_TEST_STDIO".into(),
                "scenario fixture marker".into()
            ),
            ("MCP_POOL_DEBUG".into(), "scenario override".into()),
        ]
    );
    Ok(())
}

#[cfg(unix)]
#[path = "common/fixture_lifecycle_faults.rs"]
mod faults;
