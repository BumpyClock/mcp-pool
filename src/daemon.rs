use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

use crate::config::{PoolConfig, control_socket_path};
use crate::control::{ControlRequest, ControlResponse};
use crate::diagnostics;
use crate::pool::{Pool, upstream_spec_from_def};
use crate::transport;

struct ShutdownSignal {
    shutdown: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl Drop for ShutdownSignal {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }
}

pub async fn serve() -> anyhow::Result<()> {
    diagnostics::init_from_env();

    if diagnostics::is_enabled() {
        diagnostics::set_stderr_mirror(true);
    }

    if let Err(error) = PoolConfig::load() {
        diagnostics::log(format!("config load failed at startup: {error}"));
    }

    let pool = Arc::new(Pool::new());
    let discovered = pool.discover_existing_sockets();
    diagnostics::log(format!(
        "daemon starting; discovered {discovered} existing socket(s)"
    ));

    let control_path = control_socket_path();

    let listener = transport::bind(&control_path)?;
    diagnostics::log(format!(
        "control socket bound at {}",
        control_path.display()
    ));

    {
        let warm_pool = Arc::clone(&pool);
        tokio::spawn(async move {
            match warm_pool.start_all().await {
                Ok(results) => {
                    let started = results.iter().filter(|(_, error)| error.is_none()).count();
                    diagnostics::log(format!(
                        "warmed pool: {started}/{} configured server(s) starting",
                        results.len()
                    ));
                    for (name, error) in &results {
                        if let Some(error) = error {
                            diagnostics::log(format!(
                                "warm start failed name={name} error={error}"
                            ));
                        }
                    }
                }
                Err(error) => diagnostics::log(format!("warm pool: config load failed: {error}")),
            }
        });
    }

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_notify = Arc::new(Notify::new());

    loop {
        let notified = shutdown_notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        let accepted = tokio::select! {
            _ = &mut notified => break,
            accepted = listener.accept() => accepted,
        };
        let stream = match accepted {
            Ok(stream) => stream,
            Err(error) => {
                diagnostics::log(format!("accept failed: {error}"));
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
        };

        let pool = Arc::clone(&pool);
        let shutdown = Arc::clone(&shutdown);
        let shutdown_notify = Arc::clone(&shutdown_notify);
        tokio::spawn(async move {
            handle_connection(stream, pool, shutdown, shutdown_notify).await;
        });
    }

    #[cfg(unix)]
    {
        if let Err(error) = std::fs::remove_file(&control_path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            diagnostics::log(format!("control socket cleanup failed: {error}"));
        }
    }

    Ok(())
}

/// A successful shutdown retires the pool before acknowledging and stopping accepts.
async fn handle_connection(
    stream: transport::LocalStream,
    pool: Arc<Pool>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
) {
    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    let mut line = String::new();
    let read_result = reader.read_line(&mut line).await;
    let bytes_read = match read_result {
        Ok(bytes) => bytes,
        Err(error) => {
            diagnostics::log(format!("control read failed: {error}"));
            return;
        }
    };
    if bytes_read == 0 {
        return;
    }

    let trimmed = line.trim();
    let (response, is_shutdown) = match serde_json::from_str::<ControlRequest>(trimmed) {
        Ok(request) => {
            let is_shutdown = matches!(request, ControlRequest::Shutdown);
            (dispatch(&request, &pool).await, is_shutdown)
        }
        Err(error) => {
            diagnostics::log(format!("invalid control request: {error}"));
            (
                ControlResponse::err(format!("invalid request: {error}")),
                false,
            )
        }
    };
    let _shutdown_signal = if is_shutdown && response.ok {
        Some(ShutdownSignal {
            shutdown,
            notify: shutdown_notify,
        })
    } else {
        None
    };

    let mut payload = match serde_json::to_string(&response) {
        Ok(serialized) => serialized,
        Err(error) => {
            diagnostics::log(format!("control response serialize failed: {error}"));
            return;
        }
    };
    payload.push('\n');

    if let Err(error) = write_half.write_all(payload.as_bytes()).await {
        diagnostics::log(format!("control write failed: {error}"));
        return;
    }
    if let Err(error) = write_half.flush().await {
        diagnostics::log(format!("control flush failed: {error}"));
    }
}

async fn dispatch(request: &ControlRequest, pool: &Arc<Pool>) -> ControlResponse {
    let result = match request {
        ControlRequest::StartDefinition { name, definition } => {
            let spec = upstream_spec_from_def(definition);
            pool.start(name, spec, definition.configuration_entry.clone())
                .await
        }
        ControlRequest::Start { name } => {
            let config = match PoolConfig::load() {
                Ok(config) => config,
                Err(error) => return ControlResponse::err(error.to_string()),
            };
            let Some(definition) = config.server.get(name) else {
                return ControlResponse::err(format!("unknown server: {name}"));
            };
            let spec = upstream_spec_from_def(definition);
            pool.start(name, spec, None).await
        }
        ControlRequest::StartAll => {
            return match pool.start_all().await {
                Ok(results) => {
                    let servers: Vec<serde_json::Value> = results
                        .into_iter()
                        .map(|(name, error)| match error {
                            Some(error) => {
                                serde_json::json!({ "name": name, "ok": false, "error": error })
                            }
                            None => serde_json::json!({ "name": name, "ok": true }),
                        })
                        .collect();
                    ControlResponse::data(serde_json::json!({ "servers": servers }))
                }
                Err(error) => ControlResponse::err(error.to_string()),
            };
        }
        ControlRequest::Stop { name } => pool.stop_server(name).await.map(|_| ()),
        ControlRequest::Restart { name } => pool.restart(name).await.map(|_| ()),
        ControlRequest::Status { name } => {
            let mut status = pool.get_status();
            if let Some(filter_name) = name {
                status.servers.retain(|server| &server.name == filter_name);
            }
            return match serde_json::to_value(&status) {
                Ok(value) => ControlResponse::data(value),
                Err(error) => ControlResponse::err(error.to_string()),
            };
        }
        ControlRequest::Shutdown => pool.shutdown().await,
    };
    match result {
        Ok(()) => ControlResponse::ok(),
        Err(error) => ControlResponse::err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket_proxy::retirement_tests::{RETIREMENT_ERROR, failed_retirement_pool};

    #[tokio::test]
    async fn absent_servers_and_empty_pool_shutdown_preserve_success_envelopes() {
        let pool = Arc::new(Pool::new());
        for request in [
            ControlRequest::Stop {
                name: "absent-server".into(),
            },
            ControlRequest::Restart {
                name: "absent-server".into(),
            },
            ControlRequest::Shutdown,
        ] {
            let response = dispatch(&request, &pool).await;
            assert!(response.ok);
            assert!(response.error.is_none());
            assert!(response.data.is_none());
        }
    }

    #[tokio::test]
    async fn stop_restart_and_shutdown_return_retirement_errors_to_control_clients()
    -> std::io::Result<()> {
        let pool = failed_retirement_pool().await?;
        for request in [
            ControlRequest::Stop {
                name: "failed-server".to_string(),
            },
            ControlRequest::Restart {
                name: "failed-server".to_string(),
            },
            ControlRequest::Shutdown,
        ] {
            let response = dispatch(&request, &pool).await;
            assert!(!response.ok);
            assert_eq!(response.error.as_deref(), Some(RETIREMENT_ERROR));
            assert_eq!(pool.get_status().server_count, 1);
        }
        Ok(())
    }
}
