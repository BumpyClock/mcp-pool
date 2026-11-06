use std::path::{Path, PathBuf};
use std::process::Output;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_mcp-cli");

use crate::fixture_lifecycle::{FixtureLifecycle, fixture_home};

pub struct Fixture {
    pub directory: PathBuf,
    pub project: PathBuf,
    lifecycle: FixtureLifecycle,
}

impl Fixture {
    pub fn new() -> Result<Self> {
        let directory = fixture_home::create("mi")?;
        std::fs::create_dir(directory.join("project"))?;
        let directory = std::fs::canonicalize(directory)?;
        #[cfg(windows)]
        let directory = PathBuf::from(
            directory
                .to_string_lossy()
                .strip_prefix(r"\\?\")
                .context("Windows fixture path")?,
        );
        let project = directory.join("project");
        let lifecycle = FixtureLifecycle::new(
            Path::new(BINARY),
            &directory,
            &directory.join("pool"),
            &project,
            None,
        );
        Ok(Self {
            directory,
            project,
            lifecycle,
        })
    }

    pub fn command(&self, project: &Path) -> Command {
        let mut command = Command::from(self.lifecycle.command());
        command
            .current_dir(project)
            .env("APPDATA", &self.directory)
            .env_remove("XDG_DATA_HOME")
            .env("CLAUDE_CONFIG_DIR", &self.directory)
            .env_remove("CODEX_HOME")
            .env("MCPORTER_CONFIG", self.directory.join("must-not-read.json"))
            .kill_on_drop(true);
        command
    }

    #[allow(
        dead_code,
        reason = "Some import suites supply their own command assertions"
    )]
    pub async fn run(&self, arguments: &[&str]) -> Result<Output> {
        Ok(self.command(&self.project).args(arguments).output().await?)
    }

    #[allow(
        dead_code,
        reason = "Some import suites supply their own command assertions"
    )]
    pub async fn success(&self, arguments: &[&str]) -> Result<Output> {
        let output = self.run(arguments).await?;
        anyhow::ensure!(
            output.status.success(),
            "CLI failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(output)
    }

    pub fn home_config(&self) -> PathBuf {
        self.directory
            .join("pool")
            .join("config")
            .join("config.json")
    }

    pub fn write(&self, path: &Path, value: &Value) -> Result<()> {
        std::fs::create_dir_all(path.parent().context("parent")?)?;
        std::fs::write(path, serde_json::to_vec(value)?)?;
        Ok(())
    }

    pub async fn finish(self) -> Result<()> {
        self.lifecycle.finish().await?;
        Ok(())
    }
}
