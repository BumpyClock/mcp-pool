#![allow(
    dead_code,
    reason = "Integration fixtures use different lifecycle operations"
)]

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

#[path = "fixture_environment.rs"]
pub(crate) mod fixture_environment;
#[path = "fixture_home.rs"]
pub(crate) mod fixture_home;

const POLL: Duration = Duration::from_millis(20);

pub struct FixtureLifecycle {
    executable: PathBuf,
    namespace: PathBuf,
    home: PathBuf,
    directory: PathBuf,
    config: Option<PathBuf>,
    child: Option<Child>,
    removed: bool,
}

impl FixtureLifecycle {
    pub fn new(
        executable: &Path,
        namespace: &Path,
        home: &Path,
        directory: &Path,
        config: Option<&Path>,
    ) -> Self {
        Self {
            executable: executable.to_owned(),
            namespace: namespace.to_owned(),
            home: home.to_owned(),
            directory: directory.to_owned(),
            config: config.map(Path::to_owned),
            child: None,
            removed: false,
        }
    }

    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        fixture_environment::isolate(&mut command);
        command
            .current_dir(&self.directory)
            .env("MCP_CLI_HOME", &self.home)
            .env("HOME", &self.namespace)
            .env("USERPROFILE", &self.namespace);
        if let Some(config) = &self.config {
            command.env("MCP_CLI_CONFIG", config);
        }
        command
    }

    pub async fn finish(mut self) -> io::Result<()> {
        tokio::task::spawn_blocking(move || self.retire(Duration::from_secs(20)))
            .await
            .map_err(io::Error::other)?
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "The fixture owns a child handle usable by synchronous Drop"
    )]
    pub fn start(&mut self, mut command: Command) -> io::Result<()> {
        if self.child.is_some() {
            return Err(io::Error::other("fixture daemon already started"));
        }
        self.child = Some(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
        );
        Ok(())
    }

    pub async fn ready(&mut self, budget: Duration) -> io::Result<()> {
        let mut command = self.command();
        command.args(["daemon", "status", "--json"]);
        let deadline = tokio::time::Instant::now() + budget;
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        loop {
            self.check_alive()?;
            let output = command.output();
            tokio::pin!(output);
            let output = loop {
                tokio::select! {
                    result = &mut output => break result?,
                    () = tokio::time::sleep(POLL) => self.check_alive()?,
                    () = tokio::time::sleep_until(deadline) => {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "fixture daemon readiness timed out"));
                    }
                }
            };
            self.check_alive()?;
            if output.status.success() && running(&output)? {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!(
                        "fixture daemon readiness timed out: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ),
                ));
            }
            tokio::time::sleep(POLL).await;
        }
    }

    fn check_alive(&mut self) -> io::Result<()> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("fixture daemon not started"))?;
        if let Some(status) = child.try_wait()? {
            return Err(io::Error::other(format!(
                "fixture daemon exited before readiness: {status}"
            )));
        }
        Ok(())
    }

    pub fn retire(&mut self, budget: Duration) -> io::Result<()> {
        let mut stop = self.command();
        stop.args(["daemon", "stop", "--json"]);
        let mut status = self.command();
        status.args(["daemon", "status", "--json"]);
        if self.removed {
            return Ok(());
        }
        let deadline = Instant::now() + budget;
        if running(&self.bounded_output(stop, deadline)?)? {
            return Err(io::Error::other(
                "fixture shutdown did not confirm retirement",
            ));
        }
        if let Some(child) = &mut self.child {
            loop {
                if let Some(status) = child.try_wait()? {
                    if !status.success() {
                        return Err(io::Error::other(format!("fixture daemon failed: {status}")));
                    }
                    break;
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "fixture daemon did not exit after shutdown",
                    ));
                }
                std::thread::sleep(POLL);
            }
        }
        if running(&self.bounded_output(status, deadline)?)? {
            return Err(io::Error::other(
                "fixture daemon still running after shutdown",
            ));
        }
        std::fs::remove_dir_all(&self.namespace)?;
        self.removed = true;
        Ok(())
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "Normal finish and synchronous Drop share bounded shutdown and reaping"
    )]
    fn bounded_output(&self, mut command: Command, deadline: Instant) -> io::Result<Output> {
        // Files prevent a full pipe or inherited writer from making shutdown/reaping unbounded.
        let stdout_path = self.namespace.join("fixture-shutdown.stdout");
        let stderr_path = self.namespace.join("fixture-shutdown.stderr");
        let mut child = command
            .stdin(Stdio::null())
            .stdout(std::fs::File::create(&stdout_path)?)
            .stderr(std::fs::File::create(&stderr_path)?)
            .spawn()?;
        loop {
            if let Some(status) = child.try_wait()? {
                return Ok(Output {
                    status,
                    stdout: std::fs::read(&stdout_path)?,
                    stderr: std::fs::read(&stderr_path)?,
                });
            }
            if Instant::now() >= deadline {
                // Only the command we just spawned is killed. Never signal a recorded daemon PID.
                if let Err(error) = child.kill() {
                    eprintln!("could not kill timed-out fixture control command: {error}");
                }
                let reap_deadline = Instant::now() + Duration::from_secs(1);
                while child.try_wait()?.is_none() && Instant::now() < reap_deadline {
                    std::thread::sleep(POLL);
                }
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "fixture control command timed out; namespace retained",
                ));
            }
            std::thread::sleep(POLL);
        }
    }
}

fn running(output: &Output) -> io::Result<bool> {
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "fixture control command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .map_err(io::Error::other)?
        .get("running")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| io::Error::other("malformed daemon status: missing boolean running"))
}

impl Drop for FixtureLifecycle {
    fn drop(&mut self) {
        if !self.removed
            && let Err(error) = self.retire(Duration::from_secs(20))
        {
            eprintln!(
                "fixture cleanup retained {}: {error}",
                self.namespace.display()
            );
        }
    }
}
