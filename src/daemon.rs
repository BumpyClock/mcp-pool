use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::config::{control_socket_path, PoolConfig};
use crate::control::{ControlRequest, ControlResponse};
use crate::diagnostics;
use crate::pool::{upstream_spec_from_def, Pool};
use crate::transport;

/// Long-lived daemon: binds the control socket, holds the `Pool`, and dispatches
/// control requests from CLI clients.
///
/// Framing: newline-delimited JSON. Exactly one request line in -> one response
/// line out (see control.rs contracts). Per-connection tasks share `Arc<Pool>`.
pub async fn serve() -> anyhow::Result<()> {
    diagnostics::init_from_env();

    // The daemon runs in the foreground here (whether launched directly via
    // `mcp-pool serve --debug` or auto-spawned). When debug logging is on, mirror
    // it to stderr so a terminal-run daemon shows logs live; an auto-spawned
    // daemon has stderr redirected to null, so this is harmless there.
    if diagnostics::is_enabled() {
        diagnostics::set_stderr_mirror(true);
    }

    // Validate config up front so a malformed file is surfaced immediately, but
    // keep running either way: dispatch reloads it on Start so a fixed
    // config becomes effective without restarting the daemon.
    if let Err(error) = PoolConfig::load() {
        diagnostics::log(format!("config load failed at startup: {error}"));
    }

    let pool = Arc::new(Pool::new());
    let discovered = pool.discover_existing_sockets();
    diagnostics::log(format!("daemon starting; discovered {discovered} existing socket(s)"));

    // Bind the control socket. Do NOT pre-remove a stale unix socket file here:
    // transport::bind() now distinguishes a live daemon (AddrInUse + a successful
    // connect probe means we lose and exit) from a stale leftover (probe fails, so
    // bind removes and rebinds). Unlinking first would reopen the cold-start
    // split-brain race where two daemons each unlink then bind separate listeners.
    let control_path = control_socket_path();

    let listener = transport::bind(&control_path)?;
    diagnostics::log(format!("control socket bound at {}", control_path.display()));

    // Warm the whole pool on boot: start every configured server now so their
    // upstreams boot concurrently (each in its own background task) instead of
    // lazily, one at a time, as proxy clients connect. Placed after the control
    // bind so only the singleton daemon that won the bind warms the pool.
    match pool.start_all() {
        Ok(results) => {
            let started = results.iter().filter(|(_, error)| error.is_none()).count();
            diagnostics::log(format!(
                "warmed pool: {started}/{} configured server(s) starting",
                results.len()
            ));
            for (name, error) in &results {
                if let Some(error) = error {
                    diagnostics::log(format!("warm start failed name={name} error={error}"));
                }
            }
        }
        Err(error) => diagnostics::log(format!("warm pool: config load failed: {error}")),
    }

    let shutdown = Arc::new(AtomicBool::new(false));

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }

        // Accept outside the per-connection task so a single listener serializes
        // inbound connections; each accepted stream is handled independently.
        let stream = match listener.accept().await {
            Ok(stream) => stream,
            Err(error) => {
                diagnostics::log(format!("accept failed: {error}"));
                // A transient accept error shouldn't kill the daemon. If shutdown
                // was requested concurrently the loop guard catches it next iter.
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                continue;
            }
        };

        let pool = Arc::clone(&pool);
        let shutdown = Arc::clone(&shutdown);
        tokio::spawn(async move {
            handle_connection(stream, pool, shutdown).await;
        });
    }

    // Cleanup the control socket on exit (unix). Best-effort: a failed unlink
    // shouldn't mask the real outcome.
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(&control_path);
    }

    Ok(())
}

/// Read one newline-delimited request, dispatch it, and write one response.
/// The `shutdown` flag is shared so a `Shutdown` request can signal the accept
/// loop to stop after the response is flushed.
async fn handle_connection(
    stream: transport::LocalStream,
    pool: Arc<Pool>,
    shutdown: Arc<AtomicBool>,
) {
    // Split into reader/writer so the request can be parsed incrementally while
    // the response reuses the same underlying stream.
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
        // Client connected then disconnected without sending a request.
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
            (ControlResponse::err(format!("invalid request: {error}")), false)
        }
    };

    // Serialize + write + flush BEFORE any shutdown side effects, so the client
    // always receives its ack before the listener stops accepting.
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
        return;
    }

    // Shutdown ack is now safely on the wire. Signal the accept loop and tear
    // down the pool so no new proxies accept inbound MCP traffic.
    if is_shutdown {
        shutdown.store(true, Ordering::SeqCst);
        pool.shutdown();
    }
}

/// Map a control request to its response. Pure translation: all pool mutations
/// go through the shared `Arc<Pool>`. Shutdown side effects are handled by the
/// caller after the response is flushed, so dispatch only produces the response.
async fn dispatch(request: &ControlRequest, pool: &Arc<Pool>) -> ControlResponse {
    match request {
        ControlRequest::Start { name } => {
            // Always reload config so a freshly-added server is startable without
            // restarting the daemon.
            let config = match PoolConfig::load() {
                Ok(config) => config,
                Err(error) => return ControlResponse::err(error.to_string()),
            };
            let Some(definition) = config.server.get(name) else {
                return ControlResponse::err(format!("unknown server: {name}"));
            };
            let spec = upstream_spec_from_def(definition);
            match pool.start(name, spec) {
                Ok(()) => ControlResponse::ok(),
                Err(error) => ControlResponse::err(error.to_string()),
            }
        }
        ControlRequest::StartAll => match pool.start_all() {
            Ok(results) => {
                let servers: Vec<serde_json::Value> = results
                    .into_iter()
                    .map(|(name, error)| match error {
                        Some(error) => serde_json::json!({ "name": name, "ok": false, "error": error }),
                        None => serde_json::json!({ "name": name, "ok": true }),
                    })
                    .collect();
                ControlResponse::data(serde_json::json!({ "servers": servers }))
            }
            Err(error) => ControlResponse::err(error.to_string()),
        },
        ControlRequest::Stop { name } => match pool.stop_server(name) {
            Ok(_stopped) => ControlResponse::ok(),
            Err(error) => ControlResponse::err(error.to_string()),
        },
        ControlRequest::Restart { name } => match pool.restart(name).await {
            Ok(_restarted) => ControlResponse::ok(),
            Err(error) => ControlResponse::err(error.to_string()),
        },
        ControlRequest::Status { name } => {
            let mut status = pool.get_status();
            if let Some(filter_name) = name {
                status
                    .servers
                    .retain(|server| &server.name == filter_name);
            }
            match serde_json::to_value(&status) {
                Ok(value) => ControlResponse::data(value),
                Err(error) => ControlResponse::err(error.to_string()),
            }
        }
        ControlRequest::Shutdown => ControlResponse::ok(),
    }
}
