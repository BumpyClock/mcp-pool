use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::process::Command;

const BINARY: &str = env!("CARGO_BIN_EXE_mcp-pool");
static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

pub struct Fixture {
    pub directory: PathBuf,
    pub project: PathBuf,
    finished: bool,
}

impl Fixture {
    pub fn new() -> Result<Self> {
        let directory = std::env::temp_dir().join(format!(
            "mcp-import-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(directory.join("project"))?;
        let directory = std::fs::canonicalize(directory)?;
        #[cfg(windows)]
        let directory = PathBuf::from(
            directory
                .to_string_lossy()
                .strip_prefix(r"\\?\")
                .context("Windows fixture path")?,
        );
        let project = directory.join("project");
        Ok(Self {
            directory,
            project,
            finished: false,
        })
    }

    pub fn command(&self, project: &Path) -> Command {
        let mut command = Command::new(BINARY);
        command
            .current_dir(project)
            .env("MCP_POOL_HOME", self.directory.join("pool"))
            .env("HOME", &self.directory)
            .env("USERPROFILE", &self.directory)
            .env("APPDATA", &self.directory)
            .env_remove("MCP_POOL_CONFIG")
            .env_remove("MCP_POOL_CREDENTIALS_READ_ONLY")
            .env("CLAUDE_CONFIG_DIR", &self.directory)
            .env_remove("CODEX_HOME")
            .env("MCPORTER_CONFIG", self.directory.join("must-not-read.json"))
            .kill_on_drop(true);
        command
    }

    pub async fn run(&self, arguments: &[&str]) -> Result<Output> {
        Ok(self.command(&self.project).args(arguments).output().await?)
    }

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

    pub async fn finish(mut self) -> Result<()> {
        self.success(&["daemon", "stop"]).await?;
        std::fs::remove_dir_all(&self.directory)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for Fixture {
    #[allow(
        clippy::disallowed_methods,
        reason = "Synchronous emergency cleanup after a failed assertion"
    )]
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        match std::process::Command::new(BINARY)
            .args(["daemon", "stop"])
            .current_dir(&self.project)
            .env("MCP_POOL_HOME", self.directory.join("pool"))
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(_) | Err(_) => eprintln!("Could not stop import fixture daemon"),
        }
        if let Err(error) = std::fs::remove_dir_all(&self.directory) {
            eprintln!("Could not remove import fixture: {error}");
        }
    }
}
