use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use crate::support::Fixture;

pub const CANARY: &str = "parity-ambient-value-must-not-be-copied";

pub fn write_source(path: &Path, contents: &str) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("source parent")?)?;
    std::fs::write(path, contents)?;
    Ok(())
}

pub fn read_config(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

pub fn server<'a>(document: &'a Value, name: &str) -> Result<&'a Value> {
    document
        .get("mcpServers")
        .and_then(|servers| servers.get(name))
        .with_context(|| format!("Missing imported server {name}"))
}

pub fn path_string(path: &Path) -> Result<&str> {
    path.to_str().context("fixture path is not UTF-8")
}

pub async fn run(fixture: &Fixture, arguments: &[&str]) -> Result<Output> {
    Ok(fixture
        .command(&fixture.project)
        .env("XDG_CONFIG_HOME", fixture.directory.join(".config"))
        .env_remove("OPENCODE_CONFIG")
        .env_remove("OPENCODE_CONFIG_DIR")
        .env_remove("OPENAI_WORKDIR")
        .env("CODEX_HOME", fixture.directory.join(".codex"))
        .env("PARITY_IMPORT_CANARY", CANARY)
        .args(arguments)
        .output()
        .await?)
}

pub async fn success(fixture: &Fixture, arguments: &[&str]) -> Result<Output> {
    let output = run(fixture, arguments).await?;
    ensure!(
        output.status.success(),
        "CLI failed for {arguments:?} (exit: {})",
        output.status
    );
    Ok(output)
}

pub fn no_canary(output: &Output, document: &Value) -> Result<()> {
    ensure!(
        !output
            .stdout
            .windows(CANARY.len())
            .any(|part| part == CANARY.as_bytes())
    );
    ensure!(
        !output
            .stderr
            .windows(CANARY.len())
            .any(|part| part == CANARY.as_bytes())
    );
    ensure!(!document.to_string().contains(CANARY));
    Ok(())
}

pub fn windsurf_user_path(fixture: &Fixture) -> PathBuf {
    #[cfg(windows)]
    {
        fixture
            .directory
            .join("Codeium")
            .join("windsurf")
            .join("mcp_config.json")
    }
    #[cfg(not(windows))]
    {
        fixture
            .directory
            .join(".codeium")
            .join("windsurf")
            .join("mcp_config.json")
    }
}

pub fn vscode_user_path(fixture: &Fixture) -> PathBuf {
    #[cfg(windows)]
    {
        fixture.directory.join("Code").join("User").join("mcp.json")
    }
    #[cfg(target_os = "macos")]
    {
        fixture
            .directory
            .join("Library")
            .join("Application Support")
            .join("Code")
            .join("User")
            .join("mcp.json")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        fixture
            .directory
            .join(".config")
            .join("Code")
            .join("User")
            .join("mcp.json")
    }
}

pub fn opencode_user_path(fixture: &Fixture) -> PathBuf {
    fixture
        .directory
        .join(".config")
        .join("opencode")
        .join("opencode.json")
}

pub fn cursor_user_path(fixture: &Fixture) -> PathBuf {
    fixture
        .directory
        .join(".config")
        .join("Cursor")
        .join("User")
        .join("mcp.json")
}
