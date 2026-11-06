use std::path::Path;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::{config, diagnostics, process_stdin, transport};

#[cfg(test)]
#[path = "proxy_retry_tests.rs"]
mod retry_tests;

pub async fn run_resolved(name: &str, definition: &config::ServerDef) -> anyhow::Result<()> {
    crate::daemon_client::ensure_definition_started(name, definition).await?;
    let stream = connect_with_retry(&config::server_socket_path(name)?).await?;
    bridge(stream, name).await
}

async fn bridge(stream: transport::LocalStream, name: &str) -> anyhow::Result<()> {
    let (reader, writer) = tokio::io::split(stream);
    bridge_transfers(
        process_stdin::stdin()?,
        tokio::io::stdout(),
        reader,
        writer,
        name,
    )
    .await?;
    Ok(())
}

/// Bounds the final response drain because the shared upstream outlives stdin.
async fn bridge_transfers<Input, Output, Reader, Writer>(
    input: Input,
    output: Output,
    reader: Reader,
    mut writer: Writer,
    name: &str,
) -> std::io::Result<()>
where
    Input: AsyncRead + Unpin,
    Output: AsyncWrite + Unpin,
    Reader: AsyncRead + Unpin,
    Writer: AsyncWrite + Unpin,
{
    let log = diagnostics::ServerLog::for_runtime(name);
    log.log("proxy_connected");
    let outgoing = pump(input, &mut writer, "stdin->socket", &log);
    let incoming = pump(reader, output, "socket->stdout", &log);
    tokio::pin!(outgoing, incoming);
    tokio::select! {
        result = &mut incoming => result,
        result = &mut outgoing => {
            result?;
            match tokio::time::timeout(Duration::from_millis(800), &mut incoming).await {
                Ok(result) => result,
                Err(_) => {
                    log.log("proxy_response_drain_timeout");
                    Ok(())
                }
            }
        }
    }
}

async fn connect_with_retry(path: &Path) -> std::io::Result<transport::LocalStream> {
    for _ in 0..49 {
        match transport::connect(path).await {
            Ok(stream) => return Ok(stream),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::PermissionDenied
                        | std::io::ErrorKind::InvalidInput
                        | std::io::ErrorKind::InvalidData
                        | std::io::ErrorKind::Unsupported
                ) =>
            {
                return Err(error);
            }
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    transport::connect(path).await
}

async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut reader: R,
    mut writer: W,
    direction: &str,
    log: &diagnostics::ServerLog,
) -> std::io::Result<()> {
    let mut buffer = [0u8; 8192];
    let result = async {
        loop {
            let bytes = reader.read(&mut buffer).await?;
            if bytes == 0 {
                log.log(format!("proxy_eof dir={direction}"));
                return Ok(());
            }
            writer.write_all(&buffer[..bytes]).await?;
            writer.flush().await?;
        }
    }
    .await;
    if let Err(error) = &result {
        log.log(format!(
            "proxy_transfer_failed dir={direction} error={error}"
        ));
    }
    result
}
