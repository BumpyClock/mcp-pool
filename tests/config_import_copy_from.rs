use anyhow::{Context, Result};
use serde_json::{Value, json};

#[path = "config_import_fixtures/support.rs"]
mod support;
use support::Fixture;

#[tokio::test]
async fn copy_renames_selected_import_and_merges_explicit_overrides() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("source.json");
    let original = json!({"mcpServers":{
        "original":{"command":"node","args":["old"],"env":{"KEEP":"yes","CHANGE":"old"},
            "description":"original","auth":"none","cwd":"workers"},
        "unsupported":{"type":"ws","url":"wss://example.invalid"}
    }});
    fixture.write(&source, &original)?;
    fixture
        .success(&[
            "config",
            "add",
            "alias",
            "--copy-from",
            "claude:original",
            "--from",
            source.to_str().context("source path")?,
            "--arg",
            "new",
            "--env",
            "CHANGE=new",
            "--env",
            "ADDED=value",
            "--description",
            "changed",
        ])
        .await?;
    let document: Value = serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
    assert_eq!(
        document.pointer("/mcpServers/alias/command"),
        Some(&json!(["node", "new"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/alias/env/KEEP"),
        Some(&json!("yes"))
    );
    assert_eq!(
        document.pointer("/mcpServers/alias/env/CHANGE"),
        Some(&json!("new"))
    );
    assert_eq!(
        document.pointer("/mcpServers/alias/env/ADDED"),
        Some(&json!("value"))
    );
    assert_eq!(
        document.pointer("/mcpServers/alias/description"),
        Some(&json!("changed"))
    );
    assert_eq!(
        document.pointer("/mcpServers/alias/auth"),
        Some(&json!("none"))
    );
    assert!(document.pointer("/mcpServers/original").is_none());
    assert!(document.pointer("/mcpServers/unsupported").is_none());
    assert_eq!(
        document.pointer("/mcpServers/alias/cwd"),
        Some(&json!(fixture.project.join("workers")))
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(source)?)?,
        original
    );
    fixture.finish().await
}

#[tokio::test]
async fn copy_dry_run_and_conflicts_never_expose_secret_values() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("source.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{"remote":{
            "url":"https://example.invalid/mcp",
            "headers":{"Authorization":"$env:UNSET_COPY_AUTH","X-Unset":"$env:UNSET_COPY_HEADER"},
            "env":{"REFERENCE":"$env:UNSET_COPY_ENV"}
        }}}),
    )?;
    let path = source.to_str().context("source path")?;
    let args = [
        "config",
        "add",
        "alias",
        "--copy-from",
        "mcporter:remote",
        "--from",
        path,
    ];
    let mut preview_args = args.to_vec();
    preview_args.push("--dry-run");
    let preview = fixture.success(&preview_args).await?;
    assert!(!fixture.home_config().exists());
    assert!(!String::from_utf8_lossy(&preview.stdout).contains("******"));
    assert!(!String::from_utf8_lossy(&preview.stderr).contains("******"));
    fixture.success(&args).await?;
    let original = std::fs::read(fixture.home_config())?;
    fixture.success(&args).await?;
    assert_eq!(std::fs::read(fixture.home_config())?, original);
    let mut changed = args.to_vec();
    changed.extend(["--header", "X-Added=test-header-value"]);
    let denied = fixture.run(&changed).await?;
    assert!(!denied.status.success());
    assert!(!String::from_utf8_lossy(&denied.stderr).contains("******"));
    assert_eq!(std::fs::read(fixture.home_config())?, original);
    changed.push("--replace");
    fixture.success(&changed).await?;
    let updated: Value = serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
    assert_eq!(
        updated.pointer("/mcpServers/alias/headers/Authorization"),
        Some(&json!("$env:UNSET_COPY_AUTH"))
    );
    assert_eq!(
        updated.pointer("/mcpServers/alias/headers/X-Added"),
        Some(&json!("test-header-value"))
    );
    fixture.finish().await
}

#[tokio::test]
async fn copy_rejects_static_bearer_tokens_without_writing_a_placeholder() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("source.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{"remote":{
            "url":"https://example.invalid/mcp",
            "bearerToken":"test-static-token"
        }}}),
    )?;
    let output = fixture
        .run(&[
            "config",
            "add",
            "alias",
            "--copy-from",
            "cursor:remote",
            "--from",
            source.to_str().context("source path")?,
        ])
        .await?;
    anyhow::ensure!(!output.status.success(), "static bearer token was imported");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("bearerTokenEnv"));
    assert!(!stderr.contains("test-static-token"));
    assert!(!fixture.home_config().exists());
    fixture.finish().await
}

#[tokio::test]
async fn copy_dry_run_and_write_preserve_absent_environment_references() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("source.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{"remote":{
            "url":"https://example.invalid/mcp",
            "headers":{"X-Reference":"$env:MCP_POOL_COPY_DEFINITELY_ABSENT_9F2A"},
            "env":{"REFERENCE":"${MCP_POOL_COPY_DEFINITELY_ABSENT_9F2A}"}
        }}}),
    )?;
    let path = source.to_str().context("source path")?;
    let args = [
        "config",
        "add",
        "alias",
        "--copy-from",
        "vscode:remote",
        "--from",
        path,
    ];
    let mut dry_run = args.to_vec();
    dry_run.push("--dry-run");
    let preview = fixture.success(&dry_run).await?;
    assert!(!fixture.home_config().exists());
    assert!(
        !String::from_utf8_lossy(&preview.stdout).contains("MCP_POOL_COPY_DEFINITELY_ABSENT_9F2A")
    );
    fixture.success(&args).await?;
    let document: Value = serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
    assert_eq!(
        document.pointer("/mcpServers/alias/headers/X-Reference"),
        Some(&json!("$env:MCP_POOL_COPY_DEFINITELY_ABSENT_9F2A"))
    );
    assert_eq!(
        document.pointer("/mcpServers/alias/env/REFERENCE"),
        Some(&json!("${MCP_POOL_COPY_DEFINITELY_ABSENT_9F2A}"))
    );
    fixture.finish().await
}

#[tokio::test]
async fn copy_supports_replace_and_existing_non_copy_add() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture
        .success(&["config", "add", "alias", "--command", "original"])
        .await?;
    let source = fixture.directory.join("source.toml");
    std::fs::write(
        &source,
        "[mcp_servers.remote]\nurl = \"https://example.invalid/mcp\"\n",
    )?;
    let mut args = vec![
        "config",
        "add",
        "alias",
        "--copy-from",
        "codex:remote",
        "--from",
        source.to_str().context("source path")?,
        "--header",
        "X-Key=secret",
        "--auth",
        "oauth",
    ];
    let original = std::fs::read(fixture.home_config())?;
    let denied = fixture.run(&args).await?;
    assert!(!denied.status.success());
    assert_eq!(std::fs::read(fixture.home_config())?, original);
    let mut preview = args.clone();
    preview.push("--dry-run");
    let denied_preview = fixture.run(&preview).await?;
    assert!(!denied_preview.status.success());
    assert_eq!(std::fs::read(fixture.home_config())?, original);
    args.push("--replace");
    fixture.success(&args).await?;
    let document: Value = serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
    assert_eq!(
        document.pointer("/mcpServers/alias/url"),
        Some(&json!("https://example.invalid/mcp"))
    );
    assert_eq!(
        document.pointer("/mcpServers/alias/headers/X-Key"),
        Some(&json!("secret"))
    );
    assert_eq!(
        document.pointer("/mcpServers/alias/auth"),
        Some(&json!("oauth"))
    );
    fixture.finish().await
}

#[tokio::test]
async fn copy_url_and_command_overrides_keep_the_transport_coherent() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("source.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{
            "remote":{"type":"sse","url":"https://example.invalid/old"},
            "local":{"command":"node","args":["old"]}
        }}),
    )?;
    let path = source.to_str().context("source path")?;
    fixture
        .success(&[
            "config",
            "add",
            "events",
            "--copy-from",
            "claude-code:remote",
            "--from",
            path,
            "--url",
            "https://example.invalid/new",
        ])
        .await?;
    fixture
        .success(&[
            "config",
            "add",
            "worker",
            "--copy-from",
            "claude:remote",
            "--from",
            path,
            "--command",
            "node",
            "--arg",
            "new",
        ])
        .await?;
    fixture
        .success(&[
            "config",
            "add",
            "api",
            "--copy-from",
            "claude:local",
            "--from",
            path,
            "--url",
            "https://example.invalid/mcp",
        ])
        .await?;
    let document: Value = serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
    assert_eq!(
        document.pointer("/mcpServers/events/transport"),
        Some(&json!("sse"))
    );
    assert_eq!(
        document.pointer("/mcpServers/events/baseUrl"),
        Some(&json!("https://example.invalid/new"))
    );
    assert!(document.pointer("/mcpServers/events/url").is_none());
    assert_eq!(
        document.pointer("/mcpServers/worker/transport"),
        Some(&json!("stdio"))
    );
    assert_eq!(
        document.pointer("/mcpServers/worker/command"),
        Some(&json!("node"))
    );
    assert_eq!(
        document.pointer("/mcpServers/worker/args"),
        Some(&json!(["new"]))
    );
    assert!(document.pointer("/mcpServers/worker/url").is_none());
    assert_eq!(
        document.pointer("/mcpServers/api/transport"),
        Some(&json!("http"))
    );
    assert!(document.pointer("/mcpServers/api/command").is_none());
    fixture.finish().await
}

#[tokio::test]
async fn copy_discovers_project_source_without_an_explicit_from_path() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write(
        &fixture.project.join(".mcp.json"),
        &json!({"mcpServers":{"original":{"command":"node","args":["worker.js"]}}}),
    )?;
    fixture
        .success(&[
            "config",
            "add",
            "alias",
            "--copy-from",
            "project:original",
            "--scope",
            "project",
        ])
        .await?;
    let destination = fixture.project.join(".mcp").join("config.json");
    let document: Value = serde_json::from_slice(&std::fs::read(&destination)?)?;
    assert_eq!(
        document.pointer("/mcpServers/alias/command"),
        Some(&json!(["node", "worker.js"]))
    );
    assert!(document.pointer("/mcpServers/original").is_none());
    fixture.finish().await
}

#[tokio::test]
async fn copy_and_add_preserve_protocol_version_in_config_get() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("source.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{"original":{
            "url":"https://example.invalid/mcp",
            "protocolVersion":"2026-07-28"
        }}}),
    )?;
    fixture
        .success(&[
            "config",
            "add",
            "modern",
            "--copy-from",
            "vscode:original",
            "--from",
            source.to_str().context("source path")?,
            "--protocol-version",
            "legacy",
        ])
        .await?;
    let output = fixture
        .success(&["config", "get", "modern", "--json"])
        .await?;
    let entry: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(entry.get("protocolVersion"), Some(&json!("legacy")));
    fixture.finish().await
}
