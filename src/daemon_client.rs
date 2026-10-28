use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::{self, ServerDef};
use crate::control::{ControlRequest, ControlResponse};
use crate::{diagnostics, transport};

#[path = "daemon_launch.rs"]
pub(crate) mod launch;
#[path = "daemon_configuration_ownership.rs"]
pub(crate) mod ownership;

pub(crate) async fn ensure_definition_started(
    name: &str,
    definition: &ServerDef,
) -> anyhow::Result<()> {
    require_success(
        control_request(&ControlRequest::StartDefinition {
            name: name.to_string(),
            definition: Box::new(definition.clone()),
        })
        .await?,
    )
}

fn require_success(response: ControlResponse) -> anyhow::Result<()> {
    if response.ok {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            response
                .error
                .unwrap_or_else(|| "pool startup failed".to_string())
        ))
    }
}

/// Retries once when the daemon closes the control connection without a response.
pub(crate) async fn control_request(request: &ControlRequest) -> anyhow::Result<ControlResponse> {
    request_with_eof_retry(request, ensure_daemon).await
}

async fn request_with_eof_retry<Connect, Connection>(
    request: &ControlRequest,
    mut connect: Connect,
) -> anyhow::Result<ControlResponse>
where
    Connect: FnMut() -> Connection,
    Connection: std::future::Future<Output = anyhow::Result<transport::LocalStream>>,
{
    let response = match exchange(connect().await?, request, ResponseContext::Native).await? {
        Some(response) => response,
        None => {
            diagnostics::log("control response empty; retrying once");
            exchange(connect().await?, request, ResponseContext::Native)
                .await?
                .ok_or_else(|| anyhow::anyhow!("daemon closed control socket without responding"))?
        }
    };
    Ok(response)
}

enum ResponseContext {
    Native,
    Existing,
}

async fn exchange(
    mut stream: transport::LocalStream,
    request: &ControlRequest,
    context: ResponseContext,
) -> anyhow::Result<Option<ControlResponse>> {
    let mut serialized = serde_json::to_vec(request)?;
    serialized.push(b'\n');
    stream.write_all(&serialized).await?;
    stream.flush().await?;
    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    let bytes = reader.read_line(&mut response_line).await?;
    if bytes == 0 {
        return Ok(None);
    }
    serde_json::from_str(response_line.trim())
        .map(Some)
        .map_err(|error| match context {
            ResponseContext::Native => anyhow::anyhow!("parse control response: {error}"),
            ResponseContext::Existing => {
                anyhow::Error::new(error).context("Invalid daemon control response")
            }
        })
}

/// Never launches or replays an operation whose outcome was not confirmed.
pub(crate) async fn existing_control_request(
    request: &ControlRequest,
) -> anyhow::Result<Option<ControlResponse>> {
    let path = config::control_socket_path();
    existing_request_with(
        request,
        || transport::connect(&path),
        Duration::from_secs(15),
    )
    .await
}

async fn existing_request_with<Connect, Connection>(
    request: &ControlRequest,
    mut connect: Connect,
    timeout: Duration,
) -> anyhow::Result<Option<ControlResponse>>
where
    Connect: FnMut() -> Connection,
    Connection: std::future::Future<Output = std::io::Result<transport::LocalStream>>,
{
    use anyhow::Context;
    tokio::time::timeout(timeout, async {
        let stream = loop {
            match connect().await {
                Ok(stream) => break stream,
                Err(error) if endpoint_absent(&error) => return Ok(None),
                Err(error) if endpoint_busy(&error) => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => return Err(error).context("Connecting to existing daemon"),
            }
        };
        exchange(stream, request, ResponseContext::Existing)
            .await?
            .map(Some)
            .ok_or_else(|| anyhow::anyhow!("Daemon closed without confirming the operation"))
    })
    .await
    .context("Daemon control deadline exceeded")?
}

pub(crate) fn endpoint_absent(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
    )
}

pub(crate) fn endpoint_busy(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::WouldBlock
        || (cfg!(windows) && error.raw_os_error() == Some(231))
}

async fn ensure_daemon() -> anyhow::Result<transport::LocalStream> {
    let socket_path = config::control_socket_path();
    match transport::connect(&socket_path).await {
        Ok(stream) => Ok(stream),
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Err(error.into()),
        Err(_) => {
            spawn_daemon_detached()?;
            retry_connect(&socket_path).await
        }
    }
}

async fn retry_connect(socket_path: &std::path::Path) -> anyhow::Result<transport::LocalStream> {
    let mut last_error = None;
    for _ in 0..30 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        match transport::connect(socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                return Err(error.into());
            }
            Err(error) => last_error = Some(error),
        }
    }
    let error = last_error.unwrap_or_else(|| std::io::Error::other("control socket unreachable"));
    Err(anyhow::anyhow!(
        "could not reach daemon after launch ({error}). Try `mcp-pool pool serve` manually."
    ))
}

fn spawn_daemon_detached() -> anyhow::Result<()> {
    let mut arguments = vec!["pool".into(), "serve".into()];
    if diagnostics::is_enabled() {
        arguments.push("--debug".into());
    }
    launch::spawn_detached(&arguments)?;
    diagnostics::log("spawned daemon: pool serve");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context, Result};
    use std::collections::VecDeque;
    use std::future::ready;
    use std::io;

    fn connection(
        reply: &'static [u8],
    ) -> (
        transport::LocalStream,
        tokio::task::JoinHandle<Result<ControlRequest>>,
    ) {
        let (client, server) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            let mut reader = BufReader::new(server);
            let mut request = String::new();
            reader.read_line(&mut request).await?;
            let request = serde_json::from_str(&request)?;
            reader.get_mut().write_all(reply).await?;
            reader.get_mut().flush().await?;
            Ok(request)
        });
        (Box::new(client), task)
    }

    #[tokio::test]
    async fn native_control_retries_only_an_empty_response_once() -> Result<()> {
        let (first, received_first) = connection(b"");
        let (second, received_second) = connection(b"{\"ok\":true}\n");
        let mut streams = VecDeque::from([first, second]);
        let response = request_with_eof_retry(
            &ControlRequest::Stop {
                name: "fixture".into(),
            },
            || ready(streams.pop_front().context("unexpected extra connection")),
        )
        .await?;
        assert!(response.ok);
        assert!(streams.is_empty());
        for request in [received_first.await??, received_second.await??] {
            assert!(matches!(request, ControlRequest::Stop { name } if name == "fixture"));
        }

        let (first, received_first) = connection(b"");
        let (second, received_second) = connection(b"");
        let mut streams = VecDeque::from([first, second]);
        let result = request_with_eof_retry(&ControlRequest::Shutdown, || {
            ready(streams.pop_front().context("unexpected third connection"))
        })
        .await;
        assert_eq!(
            result.err().context("expected EOF failure")?.to_string(),
            "daemon closed control socket without responding"
        );
        received_first.await??;
        received_second.await??;
        Ok(())
    }

    #[tokio::test]
    async fn malformed_response_is_not_replayed_by_either_control_policy() -> Result<()> {
        let (stream, received) = connection(b"not-json\n");
        let mut stream = Some(stream);
        let mut attempts = 0;
        let error = request_with_eof_retry(&ControlRequest::Shutdown, || {
            attempts += 1;
            ready(stream.take().context("malformed response was replayed"))
        })
        .await
        .err()
        .context("expected malformed response error")?;
        assert!(error.to_string().starts_with("parse control response: "));
        assert_eq!(attempts, 1);
        received.await??;

        let (stream, received) = connection(b"not-json\n");
        let mut stream = Some(stream);
        let mut attempts = 0;
        let result = existing_request_with(
            &ControlRequest::Shutdown,
            || {
                attempts += 1;
                ready(
                    stream
                        .take()
                        .ok_or_else(|| io::Error::other("unexpected replay")),
                )
            },
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(
            result
                .err()
                .context("expected invalid daemon response")?
                .to_string(),
            "Invalid daemon control response"
        );
        assert_eq!(attempts, 1);
        received.await??;
        Ok(())
    }

    #[tokio::test]
    async fn native_write_failure_is_not_an_empty_response_retry() -> Result<()> {
        let (client, server) = tokio::io::duplex(16);
        drop(server);
        let mut stream: Option<transport::LocalStream> = Some(Box::new(client));
        let mut attempts = 0;
        let error = request_with_eof_retry(&ControlRequest::Shutdown, || {
            attempts += 1;
            ready(stream.take().context("write failure was replayed"))
        })
        .await
        .err()
        .context("expected write error")?;
        assert_eq!(attempts, 1);
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::BrokenPipe)
        );
        Ok(())
    }

    #[tokio::test]
    async fn existing_control_does_not_replay_an_unknown_shutdown_outcome() -> Result<()> {
        let (stream, received) = connection(b"");
        let mut stream = Some(stream);
        let mut attempts = 0;
        let result = existing_request_with(
            &ControlRequest::Shutdown,
            || {
                attempts += 1;
                ready(
                    stream
                        .take()
                        .ok_or_else(|| io::Error::other("unexpected replay")),
                )
            },
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(attempts, 1);
        assert_eq!(
            result.err().context("expected EOF failure")?.to_string(),
            "Daemon closed without confirming the operation"
        );
        assert!(matches!(received.await??, ControlRequest::Shutdown));
        Ok(())
    }

    #[tokio::test]
    async fn existing_control_absence_is_a_single_connect_noop_and_permissions_are_errors()
    -> Result<()> {
        for kind in [io::ErrorKind::NotFound, io::ErrorKind::ConnectionRefused] {
            let mut attempts = 0;
            let response = existing_request_with(
                &ControlRequest::Status { name: None },
                || {
                    attempts += 1;
                    ready(Err(io::Error::from(kind)))
                },
                Duration::from_secs(1),
            )
            .await?;
            assert!(response.is_none());
            assert_eq!(attempts, 1);
        }
        let error = existing_request_with(
            &ControlRequest::Shutdown,
            || ready(Err(io::Error::from(io::ErrorKind::PermissionDenied))),
            Duration::from_secs(1),
        )
        .await
        .err()
        .context("expected permission error")?;
        assert_eq!(
            error.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(io::ErrorKind::PermissionDenied)
        );
        Ok(())
    }

    #[tokio::test]
    async fn existing_control_retries_busy_connections_before_sending_once() -> Result<()> {
        let (stream, received) = connection(b"{\"ok\":false,\"error\":\"retirement failed\"}\n");
        let mut connections = VecDeque::new();
        connections.push_back(Err(io::Error::from(io::ErrorKind::WouldBlock)));
        #[cfg(windows)]
        connections.push_back(Err(io::Error::from_raw_os_error(231)));
        connections.push_back(Ok(stream));
        let response = existing_request_with(
            &ControlRequest::Shutdown,
            || {
                ready(
                    connections
                        .pop_front()
                        .unwrap_or_else(|| Err(io::Error::other("extra connect"))),
                )
            },
            Duration::from_secs(1),
        )
        .await?
        .context("expected response")?;
        assert!(!response.ok);
        assert_eq!(response.error.as_deref(), Some("retirement failed"));
        assert!(connections.is_empty());
        assert!(matches!(received.await??, ControlRequest::Shutdown));
        Ok(())
    }

    #[tokio::test]
    async fn existing_control_deadline_covers_busy_connection_and_response_waits() -> Result<()> {
        let result = existing_request_with(
            &ControlRequest::Shutdown,
            || ready(Err(io::Error::from(io::ErrorKind::WouldBlock))),
            Duration::from_millis(5),
        )
        .await;
        assert_eq!(
            result
                .err()
                .context("expected connect deadline")?
                .to_string(),
            "Daemon control deadline exceeded"
        );

        let (client, _server) = tokio::io::duplex(4096);
        let mut stream: Option<transport::LocalStream> = Some(Box::new(client));
        let result = existing_request_with(
            &ControlRequest::Shutdown,
            || {
                ready(
                    stream
                        .take()
                        .ok_or_else(|| io::Error::other("unexpected replay")),
                )
            },
            Duration::from_millis(5),
        )
        .await;
        assert_eq!(
            result
                .err()
                .context("expected response deadline")?
                .to_string(),
            "Daemon control deadline exceeded"
        );
        Ok(())
    }
}
