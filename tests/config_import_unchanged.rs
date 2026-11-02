use anyhow::{Context, Result};
use serde_json::{Value, json};

#[path = "config_import_fixtures/preservation.rs"]
mod preservation;
#[path = "config_import_fixtures/support.rs"]
mod support;
use preservation::Snapshot;
use support::Fixture;

#[tokio::test]
async fn identical_explicit_imports_preserve_destination_bytes_and_mtime() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("source.jsonc");
    let original = "{ // Source stays untouched too.\n\"mcpServers\":{\"local\":{\"command\":\"node\",\"args\":[\"worker.js\"],\"cwd\":\"workers\"}},\n}";
    std::fs::write(&source, original)?;
    let arguments = [
        "import",
        "claude",
        "--from",
        source.to_str().context("source")?,
    ];
    fixture.success(&arguments).await?;
    let path = fixture.home_config();
    let document: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(
        document.pointer("/mcpServers/local/command"),
        Some(&json!(["node", "worker.js"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/local/cwd"),
        Some(&json!(fixture.project.join("workers")))
    );
    let snapshot = Snapshot::decorate(&path)?;
    fixture.success(&arguments).await?;
    snapshot.assert_unchanged(&path)?;
    let mut config_import = vec!["config"];
    config_import.extend(arguments);
    config_import.push("--replace");
    fixture.success(&config_import).await?;
    snapshot.assert_unchanged(&path)?;
    assert_eq!(std::fs::read_to_string(&source)?, original);

    fixture.write(
        &source,
        &json!({"mcpServers":{
            "additional":{"command":"additional"},
            "local":{"command":"changed"}
        }}),
    )?;
    let denied = fixture.run(&arguments).await?;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--replace"));
    snapshot.assert_unchanged(&path)?;
    fixture.success(&config_import).await?;
    let updated: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(
        updated.pointer("/mcpServers/local/command"),
        Some(&json!(["changed"]))
    );
    assert!(updated.pointer("/mcpServers/additional").is_some());
    fixture.finish().await
}

#[tokio::test]
async fn identical_copy_aliases_preserve_destination_bytes_and_mtime() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("source.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{"original":{"command":"node","args":["worker.js"]}}}),
    )?;
    let original = std::fs::read(&source)?;
    let arguments = [
        "config",
        "add",
        "alias",
        "--copy-from",
        "claude:original",
        "--from",
        source.to_str().context("source")?,
        "--description",
        "Copied alias",
    ];
    fixture.success(&arguments).await?;
    let path = fixture.home_config();
    let snapshot = Snapshot::decorate(&path)?;
    fixture.success(&arguments).await?;
    snapshot.assert_unchanged(&path)?;
    let mut replacement = arguments.to_vec();
    replacement.push("--replace");
    fixture.success(&replacement).await?;
    snapshot.assert_unchanged(&path)?;
    assert_eq!(std::fs::read(&source)?, original);
    replacement.extend(["--arg", "changed"]);
    fixture.success(&replacement).await?;
    let document: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(
        document.pointer("/mcpServers/alias/command"),
        Some(&json!(["node", "changed"]))
    );
    fixture.finish().await
}

#[tokio::test]
async fn ordinary_add_still_replaces_identical_entries_and_remove_still_writes() -> Result<()> {
    let fixture = Fixture::new()?;
    let arguments = ["config", "add", "local", "--command", "node"];
    fixture.success(&arguments).await?;
    let path = fixture.home_config();
    Snapshot::decorate(&path)?;
    let original = std::fs::read(&path)?;
    fixture.success(&arguments).await?;
    assert_ne!(std::fs::read(&path)?, original);
    let document: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(
        document.pointer("/mcpServers/local/command"),
        Some(&json!("node"))
    );
    fixture.success(&["config", "remove", "local"]).await?;
    let document: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
    assert_eq!(document.get("mcpServers"), Some(&json!({})));
    fixture.finish().await
}
