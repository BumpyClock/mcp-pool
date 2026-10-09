use std::io;
use std::path::PathBuf;
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

const BINARY: &str = env!("CARGO_BIN_EXE_mcp-pool");
static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

pub struct Fixture {
    pub home: PathBuf,
    pub config: PathBuf,
    pub working_directory: PathBuf,
    pub counter: PathBuf,
    daemon: Child,
    finished: bool,
}

impl Fixture {
    pub async fn new() -> io::Result<Self> {
        let home = std::env::current_dir()?
            .join(".test-artifacts")
            .join(format!(
                "mcp-cli-{}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::SeqCst)
            ));
        tokio::fs::create_dir_all(&home).await?;
        let home = tokio::fs::canonicalize(home).await?;
        let config = home.join("mcporter.json");
        let working_directory = home.join("working directory");
        tokio::fs::create_dir(&working_directory).await?;
        let counter = home.join("events.txt");
        let executable = std::env::current_exe()?;
        let arguments = [
            "--exact",
            "stdio::upstream_fixture",
            "--nocapture",
            "--quiet",
        ];
        let environment = json!({
            super::stdio::MARKER:"1", super::stdio::COUNTER:counter,
            "MCP_POOL_TEST_VALUE":"synthetic environment",
            "HOME":home, "USERPROFILE":home, "XDG_DATA_HOME":home
        });
        let contents = json!({"imports":[], "mcpServers":{
            "fixture":{"command":executable, "args":arguments,
                "env":environment, "cwd":working_directory,"lifecycle":"keep-alive"},
            "array":{"command":[executable,"--exact","stdio::upstream_fixture","--nocapture","--quiet"],
                "env":environment, "cwd":working_directory}
        }});
        tokio::fs::write(&config, contents.to_string()).await?;
        let daemon = isolated_command(&home, &config)
            .args(["pool", "serve"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let fixture = Self {
            home,
            config,
            working_directory,
            counter,
            daemon,
            finished: false,
        };
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if fixture.control_is_live().await {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .map_err(io::Error::other)?;
        Ok(fixture)
    }

    async fn control_is_live(&self) -> bool {
        #[cfg(unix)]
        {
            tokio::net::UnixStream::connect(self.home.join("state").join("mcp-pool-control.sock"))
                .await
                .is_ok()
        }
        #[cfg(windows)]
        {
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            for byte in self.home.to_string_lossy().as_bytes() {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
            tokio::net::windows::named_pipe::ClientOptions::new()
                .open(format!(r"\\.\pipe\mcp-pool-{:08x}-control", hash as u32))
                .is_ok()
        }
    }

    pub fn spawn(&self, arguments: &[&str]) -> io::Result<Child> {
        self.spawn_environment(arguments, &[])
    }

    pub fn spawn_environment(
        &self,
        arguments: &[&str],
        environment: &[(&str, &str)],
    ) -> io::Result<Child> {
        isolated_command(&self.home, &self.config)
            .args(arguments)
            .envs(environment.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }

    pub async fn command(&self, arguments: &[&str]) -> io::Result<Output> {
        self.command_input(arguments, None).await
    }

    pub async fn command_input(
        &self,
        arguments: &[&str],
        input: Option<&str>,
    ) -> io::Result<Output> {
        self.command_input_environment(arguments, input, &[]).await
    }

    pub async fn command_input_environment(
        &self,
        arguments: &[&str],
        input: Option<&str>,
        environment: &[(&str, &str)],
    ) -> io::Result<Output> {
        let mut child = self.spawn_environment(arguments, environment)?;
        let process_id = child.id();
        if let Some(input) = input {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| io::Error::other("missing command stdin"))?;
            stdin.write_all(input.as_bytes()).await?;
            stdin.shutdown().await?;
        } else {
            drop(child.stdin.take());
        }
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing command stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing command stderr"))?;
        let mut captured_stdout = Vec::new();
        let mut captured_stderr = Vec::new();
        let completed = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::try_join!(
                child.wait(),
                stdout.read_to_end(&mut captured_stdout),
                stderr.read_to_end(&mut captured_stderr)
            )
        })
        .await;
        match completed {
            Ok(result) => {
                let (status, _, _) = result?;
                Ok(Output {
                    status,
                    stdout: captured_stdout,
                    stderr: captured_stderr,
                })
            }
            Err(error) => {
                let exit_status = child.try_wait();
                let log_path = self.home.join("state").join("logs").join("mcp-pool.log");
                let diagnostics = match std::fs::read_to_string(&log_path) {
                    Ok(contents) => contents
                        .lines()
                        .rev()
                        .take(20)
                        .collect::<Vec<_>>()
                        .join("\n"),
                    Err(read_error) if read_error.kind() == io::ErrorKind::NotFound => {
                        String::new()
                    }
                    Err(read_error) => format!("could not read fixture diagnostics: {read_error}"),
                };
                Err(io::Error::other(format!(
                    "command {arguments:?} (PID {process_id:?}, home {}) timed out: {error}; exit={exit_status:?}; stdout={:?}; stderr={:?}\n{diagnostics}",
                    self.home.display(),
                    String::from_utf8_lossy(&captured_stdout),
                    String::from_utf8_lossy(&captured_stderr),
                )))
            }
        }
    }

    pub async fn success(&self, arguments: &[&str]) -> io::Result<Output> {
        let output = self.command(arguments).await?;
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        Ok(output)
    }

    pub async fn event_count(&self, event: &str) -> io::Result<usize> {
        Ok(tokio::fs::read_to_string(&self.counter)
            .await?
            .lines()
            .filter(|line| *line == event)
            .count())
    }

    pub async fn warm(&self, name: &str) -> io::Result<()> {
        self.warm_environment(name, &[]).await
    }

    pub async fn warm_environment(
        &self,
        name: &str,
        environment: &[(&str, &str)],
    ) -> io::Result<()> {
        let mut proxy =
            RpcProcess::fixture_proxy(self.spawn_environment(&["proxy", name], environment)?)?;
        let response = proxy.exchange(initialize(1)).await?;
        assert_eq!(
            response.pointer("/result/serverInfo/name"),
            Some(&json!("controlled-fixture"))
        );
        proxy.finish().await
    }

    pub async fn finish(mut self) -> io::Result<()> {
        self.success(&["shutdown"]).await?;
        let status = tokio::time::timeout(Duration::from_secs(5), self.daemon.wait())
            .await
            .map_err(io::Error::other)??;
        assert!(status.success(), "{status}");
        self.finished = true;
        tokio::fs::remove_dir_all(&self.home).await
    }
}

impl Drop for Fixture {
    #[allow(
        clippy::disallowed_methods,
        reason = "Emergency cleanup must finish synchronously; normal cleanup uses async finish"
    )]
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        match std::process::Command::new(BINARY)
            .args(["daemon", "stop"])
            .env("MCP_POOL_HOME", &self.home)
            .env("MCPORTER_CONFIG", &self.config)
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => eprintln!("fixture cleanup shutdown failed: {output:?}"),
            Err(error) => eprintln!("fixture cleanup shutdown failed: {error}"),
        }
        if let Err(error) = std::fs::remove_dir_all(&self.home) {
            eprintln!(
                "fixture cleanup could not remove {}: {error}",
                self.home.display()
            );
        }
    }
}

fn isolated_command(home: &std::path::Path, config: &std::path::Path) -> Command {
    let mut command = Command::new(BINARY);
    command
        .env("MCP_POOL_HOME", home)
        .env("MCPORTER_CONFIG", config)
        .env("MCPORTER_DAEMON_DIR", home.join("synthetic-legacy"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home)
        .env("LOCALAPPDATA", home)
        .env("XDG_CONFIG_HOME", home)
        .env("XDG_STATE_HOME", home)
        .env("XDG_DATA_HOME", home)
        .env("NO_COLOR", "1")
        .env_remove(super::stdio::MARKER)
        .env_remove("MCPORTER_KEEPALIVE")
        .env_remove("MCPORTER_DISABLE_KEEPALIVE")
        .env_remove("MCPORTER_NO_KEEPALIVE")
        .env_remove("MCPORTER_DAEMON_CHILD")
        .env_remove("MCPORTER_DAEMON_LOG")
        .env_remove("MCPORTER_DAEMON_LOG_PATH")
        .env_remove("MCPORTER_DAEMON_LOG_SERVERS")
        .env_remove("MCP_POOL_CREDENTIALS_READ_ONLY")
        .kill_on_drop(true);
    command
}

pub fn parse_json(output: &Output) -> io::Result<Value> {
    serde_json::from_slice(&output.stdout)
        .map_err(|error| io::Error::other(format!("invalid JSON output: {error}; {output:?}")))
}

pub struct RpcProcess {
    child: Child,
    input: tokio::process::ChildStdin,
    output: BufReader<tokio::process::ChildStdout>,
    fixture_preamble: bool,
}

impl RpcProcess {
    pub fn new(mut child: Child) -> io::Result<Self> {
        let input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("missing RPC stdin"))?;
        let output = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| io::Error::other("missing RPC stdout"))?,
        );
        Ok(Self {
            child,
            input,
            output,
            fixture_preamble: false,
        })
    }

    pub fn fixture_proxy(child: Child) -> io::Result<Self> {
        let mut process = Self::new(child)?;
        process.fixture_preamble = true;
        Ok(process)
    }

    pub async fn exchange(&mut self, request: Value) -> io::Result<Value> {
        self.input
            .write_all(format!("{request}\n").as_bytes())
            .await?;
        self.input.flush().await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let mut line = String::new();
                if self.output.read_line(&mut line).await? == 0 {
                    return Err(io::Error::other("RPC process closed stdout"));
                }
                let response: Value = match serde_json::from_str(&line) {
                    Ok(response) => response,
                    Err(_) if self.fixture_preamble => continue,
                    Err(error) => return Err(io::Error::other(error)),
                };
                if response.get("id") == request.get("id") {
                    self.fixture_preamble = false;
                    return Ok(response);
                }
            }
        })
        .await
        .map_err(io::Error::other)?
    }

    pub async fn finish(mut self) -> io::Result<()> {
        self.input.shutdown().await?;
        drop(self.input);
        tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .map_err(io::Error::other)??;
        Ok(())
    }
}

pub fn initialize(id: u64) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{
        "protocolVersion":"2025-06-18","capabilities":{},
        "clientInfo":{"name":"isolated-cli-test","version":"1"}
    }})
}
