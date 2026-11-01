#![cfg(unix)]

use std::io;
use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use anyhow::{Result, ensure};
use serde_json::{Value, json};
use tokio::process::Command;

#[path = "common/fixture_home.rs"]
mod fixture_home;

struct FixtureHome(PathBuf);

impl Drop for FixtureHome {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("could not remove default-path fixture home: {error}");
        }
    }
}

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mcp-pool"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("XDG_STATE_HOME", home.join("old-state"))
        .env("XDG_DATA_HOME", home.join("old-data"))
        .env("MCP_POOL_DEBUG", "1")
        .env_remove("MCP_POOL_HOME")
        .env_remove("MCP_POOL_CONFIG")
        .env_remove("MCPORTER_DAEMON_LOG")
        .env_remove("MCPORTER_DAEMON_LOG_PATH")
        .env_remove("MCPORTER_DAEMON_LOG_SERVERS")
        .kill_on_drop(true);
    command
}

async fn success(home: &Path, arguments: &[&str]) -> Result<Output> {
    let output = tokio::time::timeout(
        Duration::from_secs(15),
        command(home).args(arguments).output(),
    )
    .await??;
    ensure!(output.status.success(), "{output:?}");
    Ok(output)
}

async fn default_home_lifecycle(start_arguments: &[&str]) -> Result<()> {
    let fixture = FixtureHome(fixture_home::create("mp")?);
    let home = std::fs::canonicalize(&fixture.0)?;
    let application = home.join(".mcp");
    std::fs::create_dir(&application)?;
    std::fs::set_permissions(&application, std::fs::Permissions::from_mode(0o755))?;
    let configuration = json!({"mcpServers":{"fixture":{"command":"cat"}}}).to_string();
    let credentials =
        b"{\"version\":2,\"entries\":{},\"serverUrls\":{},\"fixtureMarker\":\"keep\"}\n";
    std::fs::write(application.join("config.json"), &configuration)?;
    std::fs::write(application.join("credentials.json"), credentials)?;

    let result: Result<()> = async {
        let listing = success(&home, &["pool", "list", "--json"]).await?;
        let listing: Value = serde_json::from_slice(&listing.stdout)?;
        let socket = listing
            .pointer("/0/socket_path")
            .and_then(Value::as_str)
            .ok_or_else(|| io::Error::other("pool listing omitted socket path"))?;
        ensure!(
            Path::new(socket).parent() == Some(application.join("run").as_path()),
            "unexpected default socket path {socket}"
        );
        ensure!(socket.len() <= 103, "default socket path is too long");
        success(&home, start_arguments).await?;
        success(&home, &["start", "fixture"]).await?;
        ensure!(std::fs::symlink_metadata(socket)?.file_type().is_socket());
        ensure!(std::fs::metadata(socket)?.permissions().mode() & 0o777 == 0o600);
        for directory in [&application, &application.join("run")] {
            ensure!(std::fs::metadata(directory)?.permissions().mode() & 0o777 == 0o700);
        }
        ensure!(
            std::fs::symlink_metadata(application.join("mcp-pool-control.sock"))?
                .file_type()
                .is_socket()
        );
        ensure!(application.join("logs/mcp-pool.log").is_file());
        ensure!(!home.join("old-state").exists());
        ensure!(!home.join("old-data").exists());
        ensure!(std::fs::read(application.join("config.json"))? == configuration.as_bytes());
        ensure!(std::fs::read(application.join("credentials.json"))? == credentials);
        Ok(())
    }
    .await;
    let shutdown = success(&home, &["daemon", "stop"]).await;
    result?;
    shutdown?;
    ensure!(!application.join("mcp-pool-control.sock").exists());
    for entry in application.join("run").read_dir()? {
        ensure!(!entry?.file_type()?.is_socket());
    }
    ensure!(std::fs::read(application.join("config.json"))? == configuration.as_bytes());
    ensure!(std::fs::read(application.join("credentials.json"))? == credentials);
    std::fs::remove_dir_all(home)?;
    Ok(())
}

#[test]
fn failed_fixture_setup_removes_its_home() -> Result<()> {
    let home = fixture_home::create("mp")?;
    let result = (|| -> Result<()> {
        let fixture = FixtureHome(home.clone());
        std::fs::write(fixture.0.join("credentials.json"), b"synthetic-credentials")?;
        anyhow::bail!("simulated fixture setup failure");
    })();
    ensure!(result.is_err());
    ensure!(!home.exists());
    Ok(())
}

#[tokio::test]
async fn native_autolaunch_repairs_existing_default_home_without_changing_config_or_credentials()
-> Result<()> {
    default_home_lifecycle(&["start", "fixture"]).await
}

#[tokio::test]
async fn explicit_daemon_start_repairs_existing_default_home_without_changing_config_or_credentials()
-> Result<()> {
    default_home_lifecycle(&["daemon", "start"]).await
}
