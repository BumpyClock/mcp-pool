use std::io;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use tokio::process::Command;

#[path = "common/fixture_home.rs"]
mod fixture_home;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Result<Self> {
        let fixture = Self(fixture_home::create("mr")?);
        std::fs::create_dir(fixture.0.join("project"))?;
        Ok(fixture)
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mcp-cli"));
        command
            .env_clear()
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("MCP_CLI_HOME", self.0.join("current"))
            .current_dir(self.0.join("project"))
            .kill_on_drop(true);
        for name in ["PATH", "SystemRoot", "WINDIR", "PATHEXT"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        command
    }

    fn configuration(&self, relative: &str, name: &str) -> Result<PathBuf> {
        self.write(
            relative,
            &json!({"mcpServers":{name:{"command":"not-executed-by-config-inspection"}}}),
        )
    }

    fn write(&self, relative: &str, value: &Value) -> Result<PathBuf> {
        let path = self.0.join(relative);
        std::fs::create_dir_all(path.parent().context("fixture file parent")?)?;
        std::fs::write(&path, serde_json::to_vec(value)?)?;
        Ok(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("could not remove rename fixture: {error}");
        }
    }
}

async fn output(command: &mut Command) -> Result<Output> {
    tokio::time::timeout(Duration::from_secs(15), command.output())
        .await
        .context("rename compatibility command timed out")?
        .context("could not run renamed CLI")
}

async fn names(command: &mut Command) -> Result<Vec<String>> {
    let output = output(command.args(["pool", "list", "--json"])).await?;
    ensure!(output.status.success(), "{output:?}");
    let entries: Value = serde_json::from_slice(&output.stdout)?;
    entries
        .as_array()
        .context("pool list must emit an array")?
        .iter()
        .map(|entry| {
            entry
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .context("pool entry omitted name")
        })
        .collect()
}

#[tokio::test]
async fn renamed_executable_and_nested_help_use_current_brand() -> Result<()> {
    let fixture = Fixture::new()?;
    let version = output(fixture.command().arg("--version")).await?;
    ensure!(version.status.success(), "{version:?}");
    ensure!(String::from_utf8(version.stdout)?.starts_with("mcp-cli "));
    for arguments in [
        vec!["--help"],
        vec!["pool", "list", "--help"],
        vec!["config", "get", "--help"],
        vec!["daemon", "start", "--help"],
    ] {
        let help = output(fixture.command().args(arguments)).await?;
        ensure!(help.status.success(), "{help:?}");
        ensure!(String::from_utf8(help.stdout)?.contains("Usage: mcp-cli"));
    }
    Ok(())
}

#[tokio::test]
async fn home_aliases_preserve_layout_and_prefer_current_setting() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configuration("legacy/config/config.json", "legacy-home")?;
    fixture.configuration("current/config/config.json", "current-home")?;
    let legacy_home = fixture.0.join("legacy");
    ensure!(
        names(
            fixture
                .command()
                .env_remove("MCP_CLI_HOME")
                .env("MCP_POOL_HOME", &legacy_home),
        )
        .await?
            == ["legacy-home"]
    );
    ensure!(names(fixture.command().env("MCP_POOL_HOME", &legacy_home)).await? == ["current-home"]);
    Ok(())
}

#[tokio::test]
async fn explicit_config_aliases_prefer_current_setting_and_cli_flag() -> Result<()> {
    let fixture = Fixture::new()?;
    let legacy = fixture.configuration("legacy.json", "legacy-config")?;
    let current = fixture.configuration("current.json", "current-config")?;
    let explicit = fixture.configuration("explicit.json", "explicit-config")?;
    ensure!(names(fixture.command().env("MCP_POOL_CONFIG", &legacy)).await? == ["legacy-config"]);
    ensure!(
        names(
            fixture
                .command()
                .env("MCP_POOL_CONFIG", &legacy)
                .env("MCP_CLI_CONFIG", &current),
        )
        .await?
            == ["current-config"]
    );
    ensure!(
        names(
            fixture
                .command()
                .env("MCP_POOL_CONFIG", &legacy)
                .env("MCP_CLI_CONFIG", &current)
                .arg("--config")
                .arg(explicit),
        )
        .await?
            == ["explicit-config"]
    );
    Ok(())
}

#[tokio::test]
async fn unset_home_override_keeps_existing_mcp_directory() -> Result<()> {
    let fixture = Fixture::new()?;
    let configuration = fixture.configuration(".mcp/config.json", "preserved-home")?;
    let before = std::fs::read(&configuration)?;
    ensure!(names(fixture.command().env_remove("MCP_CLI_HOME")).await? == ["preserved-home"]);
    ensure!(std::fs::read(configuration)? == before);
    ensure!(!fixture.0.join(".mcp-cli").exists());
    Ok(())
}

async fn read_only_rejects(command: &mut Command, credentials: &Path) -> Result<()> {
    let result = output(command).await?;
    ensure!(!result.status.success(), "{result:?}");
    ensure!(String::from_utf8(result.stderr)?.contains("read-only"));
    ensure!(
        !credentials.exists(),
        "read-only command created a credential store"
    );
    Ok(())
}

#[tokio::test]
async fn either_process_read_only_name_prevents_credential_writes() -> Result<()> {
    let fixture = Fixture::new()?;
    let configuration = fixture.write(
        "selected/config.json",
        &json!({"mcpServers":{"protected":{"url":"http://127.0.0.1:9/mcp","auth":"oauth"}}}),
    )?;
    let tokens = fixture.write(
        "synthetic-tokens.json",
        &json!({"access_token":"rename-test-synthetic","token_type":"Bearer"}),
    )?;
    for (current, legacy) in [("1", "0"), ("0", "1")] {
        read_only_rejects(
            fixture
                .command()
                .env("MCP_CLI_CONFIG", &configuration)
                .env("MCP_CLI_CREDENTIALS_READ_ONLY", current)
                .env("MCP_POOL_CREDENTIALS_READ_ONLY", legacy)
                .args(["vault", "set", "protected", "--tokens-file"])
                .arg(&tokens),
            &fixture.0.join("selected/credentials.json"),
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn configured_legacy_read_only_survives_auth_projection() -> Result<()> {
    let fixture = Fixture::new()?;
    for (current, legacy) in [("1", "0"), ("0", "1")] {
        let configuration = fixture.write(
            "selected/config.json",
            &json!({"mcpServers":{"protected":{
                "url":"http://127.0.0.1:9/mcp","auth":"oauth",
                "env":{"MCP_CLI_CREDENTIALS_READ_ONLY":current,"MCP_POOL_CREDENTIALS_READ_ONLY":legacy}
            }}}),
        )?;
        read_only_rejects(
            fixture
                .command()
                .env("MCP_CLI_CONFIG", configuration)
                .args(["config", "login", "protected", "--no-browser"]),
            &fixture.0.join("selected/credentials.json"),
        )
        .await?;
    }
    Ok(())
}

#[tokio::test]
async fn historical_native_import_selector_still_reads_old_toml() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.0.join("historical.toml");
    let original = "[server.historical]\ncommand = 'not-executed-during-import'\n";
    std::fs::write(&source, original)?;
    let imported = output(
        fixture
            .command()
            .args(["import", "mcp-pool", "--from"])
            .arg(&source)
            .args(["--dry-run", "--json"]),
    )
    .await?;
    ensure!(imported.status.success(), "{imported:?}");
    ensure!(String::from_utf8(imported.stdout)?.contains("historical"));
    ensure!(std::fs::read_to_string(source)? == original);
    ensure!(!fixture.0.join("current/config/config.json").exists());
    Ok(())
}
