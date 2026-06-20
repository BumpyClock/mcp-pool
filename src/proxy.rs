use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{config, diagnostics, transport};

/// Bridge the caller's stdio to a pooled MCP's socket. This is the per-agent
/// thin client: an agent's MCP config points at `mcp-pool proxy <name>`, and this
/// pumps stdin -> socket and socket -> stdout verbatim.
///
/// Self-starting: if the pool socket is already live the upstream is shared as-is
/// ("if it's started, proxy through"); otherwise the upstream is auto-started via
/// the daemon ("if not started, auto-start") before bridging. The agent's MCP
/// config needs no separate `start` step.
pub async fn run(name: &str) -> anyhow::Result<()> {
    let endpoint = config::server_socket_path(name);
    diagnostics::log(format!(
        "proxy_start name={} endpoint={}",
        name,
        endpoint.display()
    ));

    let stream = match transport::connect(&endpoint).await {
        Ok(stream) => {
            diagnostics::log(format!("proxy_attached_existing name={}", name));
            stream
        }
        Err(_) => {
            // Socket not live: ask the daemon to start the upstream (idempotent,
            // auto-launches the daemon), then connect to the freshly bound socket.
            diagnostics::log(format!("proxy_autostart name={}", name));
            if let Err(error) = crate::cli::ensure_started(name).await {
                eprintln!("mcp-pool: could not start pool server '{name}': {error}");
                std::process::exit(1);
            }
            match connect_with_retry(&endpoint).await {
                Ok(stream) => stream,
                Err(error) => {
                    eprintln!(
                        "mcp-pool: started '{name}' but could not connect to its pool socket ({error})"
                    );
                    std::process::exit(1);
                }
            }
        }
    };

    diagnostics::log(format!("proxy_connected name={}", name));

    let (reader, writer) = tokio::io::split(stream);
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let name_stdin = name.to_string();
    let name_stdout = name.to_string();
    let stdin_task = tokio::spawn(async move {
        pump(stdin, writer, "stdin->socket", &name_stdin).await;
    });
    let stdout_task = tokio::spawn(async move {
        pump(reader, stdout, "socket->stdout", &name_stdout).await;
    });

    // The owning agent normally terminates this process on exit. When the agent
    // closes stdin, drain any in-flight response for a brief grace window before
    // tearing down, so piped/manual use exits promptly instead of blocking on an
    // idle socket read.
    let _ = stdin_task.await;
    let _ = tokio::time::timeout(Duration::from_millis(800), stdout_task).await;
    Ok(())
}

async fn connect_with_retry(path: &Path) -> std::io::Result<transport::LocalStream> {
    // Retry briefly while the upstream finishes binding its socket. Sleep only
    // between attempts; the final attempt's real error propagates directly rather
    // than through a synthesized placeholder.
    for _ in 0..49 {
        if let Ok(stream) = transport::connect(path).await {
            return Ok(stream);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    transport::connect(path).await
}

async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    direction: &str,
    name: &str,
) {
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer).await {
            Ok(0) => {
                diagnostics::log(format!("proxy_eof name={} dir={}", name, direction));
                break;
            }
            Ok(bytes) => {
                if let Err(error) = writer.write_all(&buffer[..bytes]).await {
                    diagnostics::log(format!(
                        "proxy_write_failed name={} dir={} error={}",
                        name, direction, error
                    ));
                    break;
                }
                if let Err(error) = writer.flush().await {
                    diagnostics::log(format!(
                        "proxy_flush_failed name={} dir={} error={}",
                        name, direction, error
                    ));
                    break;
                }
            }
            Err(error) => {
                diagnostics::log(format!(
                    "proxy_read_failed name={} dir={} error={}",
                    name, direction, error
                ));
                break;
            }
        }
    }
}
