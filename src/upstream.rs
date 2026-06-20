use std::collections::BTreeMap;
use std::io;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::diagnostics;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

/// Specification of how a pooled MCP's single upstream is driven.
#[derive(Debug, Clone)]
pub enum UpstreamSpec {
    /// Local stdio command: spawn one child, own its stdin/stdout.
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    /// Remote HTTP/SSE endpoint: hold one persistent client.
    Http { url: String, sse: bool },
}

/// Handle to a running upstream. Send JSON-RPC request lines on `request_tx`;
/// the upstream emits response lines on the `response_tx` it was spawned with.
pub struct UpstreamHandle {
    pub request_tx: mpsc::Sender<String>,
}

impl UpstreamHandle {
    pub async fn spawn(
        spec: UpstreamSpec,
        response_tx: mpsc::Sender<String>,
    ) -> io::Result<UpstreamHandle> {
        match spec {
            UpstreamSpec::Stdio {
                command,
                args,
                env,
            } => Self::spawn_stdio(command, args, env, response_tx).await,
            UpstreamSpec::Http { url, sse } => Self::spawn_http(url, sse, response_tx).await,
        }
    }

    async fn spawn_stdio(
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        response_tx: mpsc::Sender<String>,
    ) -> io::Result<UpstreamHandle> {
        // On Windows, npm-style launchers ship as .cmd batch files that the
        // Rust runtime cannot resolve via Command::new("<launcher>"). Wrapping
        // in `cmd /c` lets cmd.exe resolve the .cmd shim, and CREATE_NO_WINDOW
        // keeps the child from flashing a console window.
        #[cfg(windows)]
        let mut child = {
            let mut cmd_args = vec!["/c".to_string(), command];
            cmd_args.extend(args);
            Command::new("cmd")
                .args(&cmd_args)
                .envs(env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .creation_flags(CREATE_NO_WINDOW)
                .spawn()?
        };

        #[cfg(not(windows))]
        let mut child = Command::new(&command)
            .args(&args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let (request_tx, request_rx) = mpsc::channel::<String>(1024);

        if let Some(stderr) = stderr {
            spawn_stderr_logger(stderr);
        }
        if let Some(stdout) = stdout {
            spawn_stdout_reader(stdout, response_tx);
        }
        if let Some(stdin) = stdin {
            spawn_stdin_writer(stdin, request_rx);
        }

        tokio::spawn(async move {
            let exit = child.wait().await;
            if let Ok(status) = exit {
                diagnostics::log(format!("upstream_stdio_exited status={status}"));
            } else if let Err(error) = exit {
                diagnostics::log(format!("upstream_stdio_wait_error error={error}"));
            }
        });

        Ok(UpstreamHandle { request_tx })
    }

    async fn spawn_http(
        url: String,
        sse: bool,
        response_tx: mpsc::Sender<String>,
    ) -> io::Result<UpstreamHandle> {
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .build()
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;

        let (request_tx, mut request_rx) = mpsc::channel::<String>(1024);

        tokio::spawn({
            async move {
                while let Some(line) = request_rx.recv().await {
                    if sse {
                        if let Err(error) =
                            forward_sse_request(&client, &url, &line, &response_tx).await
                        {
                            diagnostics::log(format!(
                                "upstream_sse_request_failed url={url} error={error}"
                            ));
                        }
                    } else if let Err(error) =
                        forward_json_request(&client, &url, &line, &response_tx).await
                    {
                        diagnostics::log(format!(
                            "upstream_http_request_failed url={url} error={error}"
                        ));
                    }
                }
            }
        });

        Ok(UpstreamHandle { request_tx })
    }
}

/// Read request lines from the proxy, write each as a newline-delimited frame
/// to the child's stdin. On write failure the upstream is effectively dead, so
/// we stop forwarding rather than crash the pool.
fn spawn_stdin_writer(mut stdin: tokio::process::ChildStdin, mut rx: mpsc::Receiver<String>) {
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if let Err(error) = stdin.write_all(line.as_bytes()).await {
                diagnostics::log(format!("upstream_stdin_write_error error={error}"));
                break;
            }
            if let Err(error) = stdin.write_all(b"\n").await {
                diagnostics::log(format!("upstream_stdin_write_error error={error}"));
                break;
            }
            if let Err(error) = stdin.flush().await {
                diagnostics::log(format!("upstream_stdin_flush_error error={error}"));
                break;
            }
        }
    });
}

/// Read newline-delimited JSON-RPC frames from the child's stdout and forward
/// each as a single String (one JSON object, no trailing newline) to the proxy.
fn spawn_stdout_reader(stdout: tokio::process::ChildStdout, response_tx: mpsc::Sender<String>) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdout);
        let mut buffer = String::new();
        loop {
            buffer.clear();
            match reader.read_line(&mut buffer).await {
                Ok(0) => break,
                Ok(_) => {
                    let line = buffer.trim_end_matches(['\r', '\n']);
                    if line.is_empty() {
                        continue;
                    }
                    if response_tx.send(line.to_string()).await.is_err() {
                        break;
                    }
                }
                Err(error) => {
                    diagnostics::log(format!("upstream_stdout_read_error error={error}"));
                    break;
                }
            }
        }
        diagnostics::log("upstream_stdout_closed");
    });
}

/// Capture child stderr into the diagnostics log so launches that fail after
/// spawn (missing binary, bad args) leave a trail without polluting stdout.
fn spawn_stderr_logger(stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut buffer = String::new();
        loop {
            buffer.clear();
            match reader.read_line(&mut buffer).await {
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = buffer.trim_end_matches(['\r', '\n']);
                    if !trimmed.is_empty() {
                        diagnostics::log(format!("upstream_stderr {trimmed}"));
                    }
                }
                Err(error) => {
                    diagnostics::log(format!("upstream_stderr_read_error error={error}"));
                    break;
                }
            }
        }
    });
}

async fn forward_json_request(
    client: &reqwest::Client,
    url: &str,
    line: &str,
    response_tx: &mpsc::Sender<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(line.to_string())
        .send()
        .await?;

    let content_type = content_type_root(response.headers());

    if content_type == "text/event-stream" {
        stream_sse(response, response_tx).await?;
        return Ok(());
    }

    let body = response.text().await?;
    let trimmed = body.trim_end_matches(['\r', '\n']);
    if !trimmed.is_empty() && response_tx.send(trimmed.to_string()).await.is_err() {
        return Ok(());
    }
    Ok(())
}

async fn forward_sse_request(
    client: &reqwest::Client,
    url: &str,
    line: &str,
    response_tx: &mpsc::Sender<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let response = client
        .post(url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .body(line.to_string())
        .send()
        .await?;

    stream_sse(response, response_tx).await
}

/// Parse an SSE response body, emitting each `data:` payload as its own String.
/// Ignores comments (`:`), event framing, and `[DONE]` sentinels.
///
/// The body is buffered in full before parsing. MCP servers emit a bounded
/// stream of SSE events per response rather than holding a persistent stream,
/// so buffering does not risk unbounded memory growth. Keeping the body local
/// avoids pulling in a streaming `StreamExt` dependency.
async fn stream_sse(
    response: reqwest::Response,
    response_tx: &mpsc::Sender<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let body = response.bytes().await?;
    let text = String::from_utf8_lossy(&body);

    for event_block in text.split("\n\n") {
        for payload in extract_sse_data_payloads(event_block) {
            if response_tx.send(payload).await.is_err() {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn extract_sse_data_payloads(block: &str) -> Vec<String> {
    // Multi-line `data:` fields concatenate with a newline between them per the
    //SSE spec; an MCP server only ever sends single-line JSON payloads, so we
    // join the gathered data lines and treat the join as one JSON-RPC object.
    let mut data_lines: Vec<&str> = Vec::new();
    for raw_line in block.lines() {
        if raw_line.is_empty() || raw_line.starts_with(':') {
            continue;
        }
        let Some((field, value)) = raw_line.split_once(':') else {
            continue;
        };
        if field.trim() != "data" {
            continue;
        }
        // Leading single space after the colon is conventional and trimmed.
        let trimmed_value = value.strip_prefix(' ').unwrap_or(value);
        if trimmed_value == "[DONE]" {
            continue;
        }
        data_lines.push(trimmed_value);
    }

    if data_lines.is_empty() {
        return Vec::new();
    }

    let joined = data_lines.join("\n");
    if joined.is_empty() {
        return Vec::new();
    }
    vec![joined]
}

fn content_type_root(headers: &reqwest::header::HeaderMap) -> String {
    headers
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|raw| {
            let main = raw.split(';').next().unwrap_or(raw);
            main.trim().to_ascii_lowercase()
        })
        .unwrap_or_default()
}
