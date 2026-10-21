use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{config, diagnostics, transport};

pub async fn run_resolved(name: &str, definition: &config::ServerDef) -> anyhow::Result<()> {
    crate::daemon_client::ensure_definition_started(name, definition).await?;
    let stream = connect_with_retry(&config::server_socket_path(name)).await?;
    bridge(stream, name).await
}

/// Drains responses for up to 800 ms after stdin closes so pending replies can finish.
async fn bridge(stream: transport::LocalStream, name: &str) -> anyhow::Result<()> {
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

    let _ = stdin_task.await;
    let _ = tokio::time::timeout(Duration::from_millis(800), stdout_task).await;
    Ok(())
}

async fn connect_with_retry(path: &Path) -> std::io::Result<transport::LocalStream> {
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
