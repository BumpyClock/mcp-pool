use std::collections::BTreeMap;
use std::io;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, oneshot, watch};

use crate::diagnostics;
use crate::upstream::UpstreamHandle;
use crate::upstream_process::OwnedProcess;

const STDOUT_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

#[cfg(test)]
pub async fn spawn(
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    response_tx: mpsc::Sender<String>,
) -> io::Result<UpstreamHandle> {
    spawn_with_cwd(command, args, env, None, response_tx).await
}

/// The configured environment overlays, rather than replaces, the parent's environment.
#[cfg(test)]
pub async fn spawn_with_cwd(
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<std::path::PathBuf>,
    response_tx: mpsc::Sender<String>,
) -> io::Result<UpstreamHandle> {
    spawn_configured(command, args, env, cwd, false, response_tx).await
}

pub async fn spawn_configured(
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: Option<std::path::PathBuf>,
    clear_env: bool,
    response_tx: mpsc::Sender<String>,
) -> io::Result<UpstreamHandle> {
    #[cfg(windows)]
    let mut launch = {
        // Rust selects cmd.exe and its batch-specific encoder only for .cmd/.bat.
        let mut launch = Command::new(resolve_windows_command(
            &command,
            &env,
            cwd.as_deref(),
            clear_env,
        )?);
        launch.args(args);
        launch
    };
    #[cfg(unix)]
    let mut launch = {
        let mut launch = Command::new(command);
        launch.args(args);
        launch
    };
    if clear_env {
        launch.env_clear();
    }
    launch
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = cwd {
        launch.current_dir(cwd);
    }
    let mut process = OwnedProcess::spawn(launch).await?;
    let pipes = (
        process.child.stdin.take(),
        process.child.stdout.take(),
        process.child.stderr.take(),
    );
    let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
        process.retire().await.map_err(|error| {
            io::Error::new(
                io::ErrorKind::ResourceBusy,
                format!("upstream pipe setup failed; retirement was not confirmed: {error}"),
            )
        })?;
        return Err(io::Error::other(
            "upstream standard pipes were not established",
        ));
    };
    let (request_tx, request_rx) = mpsc::channel(1024);
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let (completion_tx, completion_rx) = watch::channel(None);
    tokio::spawn(async move {
        let result = {
            let responses = read_responses(stdout, response_tx);
            tokio::pin!(responses);
            let natural_exit = {
                let requests = write_requests(stdin, request_rx);
                let errors = async move {
                    read_stderr(stderr).await?;
                    // Closing stderr alone is valid and must not stop the upstream.
                    std::future::pending::<io::Result<()>>().await
                };
                tokio::select! {
                    _ = &mut shutdown_rx => false,
                    result = process.wait_for_exit() => {
                        let exited = result.is_ok();
                        log_worker_result("wait", result);
                        exited
                    }
                    result = requests => {
                        log_worker_result("stdin", result);
                        false
                    }
                    result = &mut responses => {
                        log_worker_result("stdout", result);
                        false
                    }
                    result = errors => {
                        log_worker_result("stderr", result);
                        false
                    }
                }
            };
            let result = process.retire().await.map_err(|error| error.to_string());
            if natural_exit && result.is_ok() {
                // Retiring descendants closes inherited stdout writers first.
                // Bound forwarding so a full response queue cannot hold completion.
                match tokio::time::timeout(STDOUT_DRAIN_TIMEOUT, &mut responses).await {
                    Ok(result) => log_worker_result("stdout_drain", result),
                    Err(_) => diagnostics::log("upstream_stdout_drain_timeout"),
                }
            }
            result
        };
        if let Err(error) = &result {
            diagnostics::log(format!("upstream_retirement_unverified error={error}"));
        } else {
            diagnostics::log("upstream_stdio_retired");
        }
        completion_tx.send_replace(Some(result));
    });
    Ok(UpstreamHandle::new(request_tx, shutdown_tx, completion_rx))
}

fn log_worker_result(worker: &str, result: io::Result<()>) {
    if let Err(error) = result {
        diagnostics::log(format!("upstream_{worker}_error error={error}"));
    }
}

async fn write_requests(
    mut stdin: ChildStdin,
    mut requests: mpsc::Receiver<crate::upstream::UpstreamRequest>,
) -> io::Result<()> {
    while let Some(line) = requests.recv().await {
        stdin.write_all(line.as_bytes()).await?;
        stdin.write_all(b"\n").await?;
        stdin.flush().await?;
    }
    Ok(())
}

async fn read_responses(stdout: ChildStdout, responses: mpsc::Sender<String>) -> io::Result<()> {
    let mut reader = BufReader::new(stdout);
    let mut buffer = String::new();
    let mut forwarded: u64 = 0;
    loop {
        buffer.clear();
        let length = tokio::select! {
            result = reader.read_line(&mut buffer) => result?,
            _ = responses.closed() => return Ok(()),
        };
        if length == 0 {
            return Ok(());
        }
        let line = buffer.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            continue;
        }
        match responses.try_send(line.to_string()) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(payload)) => {
                let blocked_since = std::time::Instant::now();
                diagnostics::log(format!(
                    "upstream_stdout_backpressure response_channel_full available={}",
                    responses.capacity()
                ));
                if responses.send(payload).await.is_err() {
                    return Ok(());
                }
                diagnostics::log(format!(
                    "upstream_stdout_unblocked blocked_ms={}",
                    blocked_since.elapsed().as_millis()
                ));
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return Ok(()),
        }
        forwarded += 1;
        if forwarded.is_multiple_of(1000) {
            diagnostics::log(format!(
                "upstream_stdout_gauge forwarded={forwarded} channel_available={}",
                responses.capacity()
            ));
        }
    }
}

async fn read_stderr(stderr: ChildStderr) -> io::Result<()> {
    let raw = std::env::var("MCP_POOL_RAW_UPSTREAM_STDERR")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes"))
        .unwrap_or(false);
    let mut reader = BufReader::new(stderr);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer).await? == 0 {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&buffer);
        let trimmed = text.trim_end_matches(['\r', '\n']);
        if !trimmed.is_empty() {
            let line = if raw {
                trimmed.to_string()
            } else {
                diagnostics::summarize_log_line(trimmed)
            };
            diagnostics::log(format!("upstream_stderr {line}"));
        }
    }
}

#[cfg(windows)]
pub(crate) fn resolve_windows_command(
    command: &str,
    environment: &BTreeMap<String, String>,
    cwd: Option<&std::path::Path>,
    clear_env: bool,
) -> io::Result<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    let variable = |name: &str| -> Option<OsString> {
        environment
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| OsString::from(value))
            .or_else(|| {
                if clear_env {
                    None
                } else {
                    std::env::var_os(name)
                }
            })
    };
    let path = Path::new(command);
    let directory = match cwd {
        Some(path) => std::path::absolute(path)?,
        None => std::env::current_dir()?,
    };
    let resolve = |path: PathBuf| {
        if path.is_absolute() {
            path
        } else {
            directory.join(path)
        }
    };
    let candidates: Vec<PathBuf> = if path.components().count() > 1 || path.is_absolute() {
        vec![resolve(path.to_path_buf())]
    } else {
        std::iter::once(directory.join(command))
            .chain(variable("PATH").into_iter().flat_map(|value| {
                std::env::split_paths(&value)
                    .map(|directory| resolve(directory.join(command)))
                    .collect::<Vec<_>>()
            }))
            .collect()
    };
    let extensions = variable("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
    for candidate in candidates {
        if candidate.is_file() {
            return std::path::absolute(candidate);
        }
        if candidate.extension().is_none() {
            for extension in extensions.to_string_lossy().split(';') {
                let mut executable = candidate.as_os_str().to_os_string();
                executable.push(extension);
                if Path::new(&executable).is_file() {
                    return std::path::absolute(executable);
                }
            }
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        "upstream executable was not found",
    ))
}

#[cfg(test)]
#[path = "upstream_process_tests.rs"]
mod tests;

#[cfg(test)]
mod options_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn relative_executable_is_resolved_from_configured_cwd() -> io::Result<()> {
        let directory =
            std::path::PathBuf::from(std::env::var("SystemRoot").map_err(io::Error::other)?)
                .join("System32");
        assert_eq!(
            resolve_windows_command(".\\cmd.exe", &BTreeMap::new(), Some(&directory), false)?,
            directory.join("cmd.exe")
        );
        Ok(())
    }

    #[tokio::test]
    async fn configured_cwd_and_env_preserve_inherited_environment() -> io::Result<()> {
        let directory = std::env::current_dir()?.join("src");
        #[cfg(windows)]
        let (command, arguments, inherited) = (
            "powershell.exe".to_string(),
            vec![
                "-NoProfile".into(),
                "-Command".into(),
                "[Console]::WriteLine((Get-Location).Path); [Console]::WriteLine($env:MCP_POOL_TRANSPORT_TEST_VALUE); [Console]::WriteLine($env:SystemRoot)".into(),
            ],
            std::env::var("SystemRoot").map_err(io::Error::other)?,
        );
        #[cfg(unix)]
        let (command, arguments, inherited) = (
            "sh".to_string(),
            vec![
                "-c".into(),
                "pwd; printf '%s\\n' \"$MCP_POOL_TRANSPORT_TEST_VALUE\" \"$HOME\"".into(),
            ],
            std::env::var("HOME").map_err(io::Error::other)?,
        );
        let environment = BTreeMap::from([(
            "MCP_POOL_TRANSPORT_TEST_VALUE".into(),
            "synthetic-env-value".into(),
        )]);
        let (responses, mut receiver) = mpsc::channel(8);
        let mut handle = spawn_with_cwd(
            command,
            arguments,
            environment,
            Some(directory.clone()),
            responses,
        )
        .await?;
        for expected in [
            directory.to_string_lossy().into_owned(),
            "synthetic-env-value".into(),
            inherited,
        ] {
            let response = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .map_err(io::Error::other)?
                .ok_or_else(|| io::Error::other("stdio fixture ended before output"))?;
            assert_eq!(response, expected);
        }
        handle.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn clear_env_uses_only_the_captured_environment() -> io::Result<()> {
        #[cfg(windows)]
        let (command, arguments, absent) = {
            assert!(std::env::var_os("USERPROFILE").is_some());
            let command =
                std::path::PathBuf::from(std::env::var("SystemRoot").map_err(io::Error::other)?)
                    .join("System32")
                    .join("cmd.exe")
                    .to_string_lossy()
                    .into_owned();
            (
                command,
                vec![
                    "/d".into(),
                    "/c".into(),
                    "echo %MCP_POOL_TRANSPORT_TEST_VALUE%&echo %USERPROFILE%".into(),
                ],
                "%USERPROFILE%",
            )
        };
        #[cfg(unix)]
        let (command, arguments, absent) = {
            assert!(std::env::var_os("HOME").is_some());
            (
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    "printf '%s\\n' \"$MCP_POOL_TRANSPORT_TEST_VALUE\" \"${HOME-unset}\"".into(),
                ],
                "unset",
            )
        };
        let (responses, mut receiver) = mpsc::channel(4);
        let environment = BTreeMap::from([(
            "MCP_POOL_TRANSPORT_TEST_VALUE".into(),
            "synthetic-captured-value".into(),
        )]);
        let mut handle =
            spawn_configured(command, arguments, environment, None, true, responses).await?;
        for expected in ["synthetic-captured-value", absent] {
            let response = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .map_err(io::Error::other)?
                .ok_or_else(|| {
                    io::Error::other("captured-environment fixture ended before output")
                })?;
            assert_eq!(response, expected);
        }
        handle.shutdown().await?;
        Ok(())
    }
}
