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

pub async fn spawn(
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    response_tx: mpsc::Sender<String>,
) -> io::Result<UpstreamHandle> {
    #[cfg(windows)]
    let mut launch = {
        // Rust selects cmd.exe and its batch-specific encoder only for .cmd/.bat.
        let mut launch = Command::new(resolve_windows_command(&command, &env)?);
        launch.args(args);
        launch
    };
    #[cfg(unix)]
    let mut launch = {
        let mut launch = Command::new(command);
        launch.args(args);
        launch
    };
    launch
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    mut requests: mpsc::Receiver<String>,
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
fn resolve_windows_command(
    command: &str,
    environment: &BTreeMap<String, String>,
) -> io::Result<std::path::PathBuf> {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};

    let variable = |name: &str| -> Option<OsString> {
        environment
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| OsString::from(value))
            .or_else(|| std::env::var_os(name))
    };
    let path = Path::new(command);
    let candidates: Vec<PathBuf> = if path.components().count() > 1 || path.is_absolute() {
        vec![path.to_path_buf()]
    } else {
        std::iter::once(PathBuf::from(command))
            .chain(variable("PATH").into_iter().flat_map(|value| {
                std::env::split_paths(&value)
                    .map(|directory| directory.join(command))
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
