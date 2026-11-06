#[path = "common/fixture_lifecycle.rs"]
mod fixture_lifecycle;

use anyhow::{Context, Result};
use serde_json::{Value, json};

#[path = "mcp_cli_fixtures/http.rs"]
#[allow(dead_code)]
mod http;
#[path = "config_import_fixtures/support.rs"]
mod support;
use support::Fixture;

#[tokio::test]
async fn import_copies_claude_project_servers_without_touching_the_source() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.project.join(".mcp.json");
    let original = r#"{
      // A server with credentials that are not available during import.
      "mcpServers": {
        "docs": {"type":"sse","url":"https://example.invalid/events","headers":{"X-Key":"${UNSET_IMPORT_KEY}"}},
        "local": {"command":"node","args":["${UNSET_IMPORT_ARG}"],"cwd":"workers"}
      },
    }"#;
    std::fs::write(&source, original)?;
    let preview = fixture
        .success(&[
            "import",
            "project",
            "--scope",
            "project",
            "--dry-run",
            "--json",
        ])
        .await?;
    assert!(!fixture.project.join(".mcp").exists());
    let preview: Value = serde_json::from_slice(&preview.stdout)?;
    assert_eq!(preview.get("dryRun"), Some(&json!(true)));
    fixture
        .success(&["config", "import", "project", "--scope", "project"])
        .await?;
    assert_eq!(std::fs::read_to_string(&source)?, original);
    let destination = fixture.project.join(".mcp").join("config.json");
    let document: Value = serde_json::from_slice(&std::fs::read(&destination)?)?;
    assert_eq!(
        document.pointer("/mcpServers/docs/transport"),
        Some(&json!("sse"))
    );
    assert_eq!(
        document.pointer("/mcpServers/docs/headers/X-Key"),
        Some(&json!("${UNSET_IMPORT_KEY}"))
    );
    assert_eq!(
        document.pointer("/mcpServers/local/command"),
        Some(&json!(["node", "${UNSET_IMPORT_ARG}"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/local/cwd"),
        Some(&json!(fixture.project.join("workers")))
    );
    fixture
        .success(&["import", "project", "--scope", "project"])
        .await?;
    fixture.finish().await
}

#[tokio::test]
async fn home_and_project_merge_before_environment_resolution_and_explicit_config_is_isolated()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write(
        &fixture.home_config(),
        &json!({"mcpServers":{
            "home":{"command":"home-fixture"},
            "same":{"command":"${MCP_CLI_INTENTIONALLY_UNSET_OVERRIDDEN}"}
        }}),
    )?;
    fixture.write(
        &fixture.project.join(".mcp").join("config.json"),
        &json!({"mcpServers":{
            "same":{"command":"project-fixture"},"local":{"command":"local-fixture"}
        }}),
    )?;
    let list: Value =
        serde_json::from_slice(&fixture.success(&["config", "list", "--json"]).await?.stdout)?;
    assert_eq!(
        list.get("servers").and_then(Value::as_array).map(Vec::len),
        Some(3)
    );
    let definition: Value = serde_json::from_slice(
        &fixture
            .success(&["config", "get", "same", "--json"])
            .await?
            .stdout,
    )?;
    assert_eq!(definition.get("command"), Some(&json!("project-fixture")));
    let native: Value =
        serde_json::from_slice(&fixture.success(&["pool", "list", "--json"]).await?.stdout)?;
    assert_eq!(native.as_array().map(Vec::len), Some(3));
    let explicit = fixture.directory.join("explicit.json");
    fixture.write(
        &explicit,
        &json!({"mcpServers":{"only":{"command":"explicit-fixture"}}}),
    )?;
    let explicit: Value = serde_json::from_slice(
        &fixture
            .success(&[
                "--config",
                explicit.to_str().context("path")?,
                "config",
                "list",
                "--json",
            ])
            .await?
            .stdout,
    )?;
    assert_eq!(
        explicit
            .get("servers")
            .and_then(Value::as_array)
            .map(Vec::len),
        Some(1)
    );
    fixture.finish().await
}

#[tokio::test]
async fn codex_import_converts_headers_filters_and_timeouts() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("codex.toml");
    std::fs::write(
        &source,
        r#"
[mcp_servers.docs]
url = "https://example.invalid/mcp"
bearer_token_env_var = "CODEX_TEST_TOKEN"
tool_timeout_sec = 2
enabled_tools = ["read", "write"]
disabled_tools = ["write"]
[mcp_servers.docs.env_http_headers]
X-Key = "CODEX_TEST_KEY"
[mcp_servers.disabled]
command = "not-imported"
enabled = false
"#,
    )?;
    fixture
        .success(&[
            "import",
            "codex",
            "--from",
            source.to_str().context("path")?,
        ])
        .await?;
    let document: Value = serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
    assert_eq!(
        document.pointer("/mcpServers/docs/headers/X-Key"),
        Some(&json!("$env:CODEX_TEST_KEY"))
    );
    assert_eq!(
        document.pointer("/mcpServers/docs/bearerTokenEnv"),
        Some(&json!("CODEX_TEST_TOKEN"))
    );
    assert_eq!(
        document.pointer("/mcpServers/docs/timeoutMs"),
        Some(&json!(2000))
    );
    assert_eq!(
        document.pointer("/mcpServers/docs/allowedTools"),
        Some(&json!(["read"]))
    );
    assert!(document.pointer("/mcpServers/disabled").is_none());
    fixture.finish().await
}

#[tokio::test]
async fn claude_discovery_respects_user_project_and_local_precedence() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write(&fixture.directory.join(".claude.json"), &json!({
        "mcpServers":{"home":{"command":"user-home"},"shared":{"command":"user-shared"}},
        "projects":{fixture.project.to_string_lossy().as_ref():{"mcpServers":{"shared":{"command":"local-shared"}}}}
    }))?;
    fixture.write(
        &fixture.project.join(".mcp.json"),
        &json!({"mcpServers":{
            "shared":{"command":"project-shared"},"project":{"command":"project-only"}
        }}),
    )?;
    fixture.success(&["import", "claude"]).await?;
    let document: Value = serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
    assert_eq!(
        document.pointer("/mcpServers/home/command"),
        Some(&json!(["user-home"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/shared/command"),
        Some(&json!(["local-shared"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/project/command"),
        Some(&json!(["project-only"]))
    );
    fixture.finish().await
}

#[tokio::test]
async fn project_cross_tool_conflicts_do_not_create_a_destination() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write(
        &fixture.project.join(".mcp.json"),
        &json!({"mcpServers":{"same":{"command":"claude"}}}),
    )?;
    let codex = fixture.project.join(".codex");
    std::fs::create_dir(&codex)?;
    std::fs::write(
        codex.join("config.toml"),
        "[mcp_servers.same]\ncommand = \"codex\"\n",
    )?;
    let result = fixture
        .run(&["import", "project", "--scope", "project"])
        .await?;
    assert!(!result.status.success());
    assert!(!fixture.project.join(".mcp").exists());
    assert!(!fixture.home_config().exists());
    fixture.finish().await
}

#[tokio::test]
async fn legacy_native_toml_is_only_read_by_explicit_import() -> Result<()> {
    let fixture = Fixture::new()?;
    let legacy = fixture.directory.join("native.toml");
    std::fs::write(
        &legacy,
        "[server.old]\ncommand = \"node\"\nargs = [\"server.js\"]\n",
    )?;
    let path = legacy.to_str().context("legacy path")?;
    fixture
        .success(&["import", "mcp-pool", "--from", path])
        .await?;
    let output: Value =
        serde_json::from_slice(&fixture.success(&["pool", "list", "--json"]).await?.stdout)?;
    assert_eq!(output.as_array().map(Vec::len), Some(1));
    std::fs::remove_file(legacy)?;
    fixture.success(&["config", "get", "old", "--json"]).await?;
    fixture.finish().await
}

#[tokio::test]
async fn selected_imports_ignore_unsupported_unselected_servers() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("selected.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{
            "good":{"command":"node"},
            "unsupported":{"type":"ws","url":"wss://example.invalid"}
        }}),
    )?;
    fixture
        .success(&[
            "import",
            "claude",
            "good",
            "--from",
            source.to_str().context("source")?,
        ])
        .await?;
    let document: Value = serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
    assert!(document.pointer("/mcpServers/good").is_some());
    assert!(document.pointer("/mcpServers/unsupported").is_none());
    fixture.finish().await
}

#[tokio::test]
async fn start_all_reports_failure_and_project_native_starts_use_the_callers_config() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.write(
        &fixture.home_config(),
        &json!({"mcpServers":{"broken":{"command":"mcp-cli-intentionally-absent-executable"}}}),
    )?;
    let start = fixture.run(&["start"]).await?;
    assert!(!start.status.success());
    assert!(String::from_utf8_lossy(&start.stdout).contains("broken"));
    let remote = http::HttpFixture::new().await?;
    fixture.write(
        &fixture.project.join(".mcp").join("config.json"),
        &json!({"mcpServers":{
            "broken":{"url":remote.url}
        }}),
    )?;
    fixture.success(&["start", "broken"]).await?;
    let status: Value = serde_json::from_slice(
        &fixture
            .success(&["status", "broken", "--json"])
            .await?
            .stdout,
    )?;
    assert_eq!(status.pointer("/servers/0/transport"), Some(&json!("http")));
    fixture.finish().await
}
