use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio::time::{Duration, sleep};

use crate::diagnostics;
use crate::jsonrpc::{self, IdAllocator};
use crate::transport::{LocalListener, LocalStream};
use crate::types::ServerStatus;
use crate::upstream::{UpstreamHandle, UpstreamSpec};

// Throttle/expire the pending-request map opportunistically rather than on a timer.
const REQUEST_TTL_SECS: u64 = 300;
const CLEANUP_INTERVAL: u32 = 100;

type ClientSender = mpsc::Sender<String>;

/// A request awaiting its response: the client that issued it, the client's
/// original JSON-RPC id (restored on the matching response), and when it was
/// registered (for TTL cleanup). Keyed in the request map by the pool-unique id.
type PendingRequest = (String, Value, Instant);
type RequestMap = Arc<Mutex<HashMap<String, PendingRequest>>>;

/// One pooled MCP: a single upstream multiplexed across many agent clients.
/// Clients connect over the bound local socket; requests forward via
/// `UpstreamHandle` and responses route back by JSON-RPC `id`. Id-less
/// responses broadcast to all connected clients.
pub struct SocketProxy {
    name: String,
    socket_path: PathBuf,
    spec: UpstreamSpec,
    owned: bool,
    status: Arc<Mutex<ServerStatus>>,
    // Upstream request sender, set once `UpstreamHandle::spawn` resolves in a
    // background task. Mutex<Option<..>> so handle_client can clone-or-skip.
    request_tx: Arc<Mutex<Option<mpsc::Sender<String>>>>,
    listener: Mutex<Option<Arc<LocalListener>>>,
    clients: Arc<Mutex<HashMap<String, ClientSender>>>,
    request_map: RequestMap,
    // Per-upstream id translation: client request ids are rewritten to pool-unique
    // ids before forwarding, so concurrent clients (which independently reuse
    // 1, 2, 3, ...) never collide on the shared upstream connection.
    id_allocator: Arc<IdAllocator>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    ready_notify: Arc<Notify>,
    // Fires when the upstream request sender is published (or when the upstream
    // stops), so a client that connects mid-startup waits for the sender instead
    // of dropping its first request.
    upstream_ready: Arc<Notify>,
    started_at: Mutex<Option<Instant>>,
    total_connections: Arc<AtomicU32>,
    cleanup_counter: Arc<AtomicU32>,
    // Fired when the upstream task exits, so `restart` can await a clean stop.
    exit_complete_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    exit_complete_rx: Mutex<Option<oneshot::Receiver<()>>>,
}

impl SocketProxy {
    pub fn new(name: String, socket_path: PathBuf, spec: UpstreamSpec, owned: bool) -> Self {
        Self {
            name, socket_path, spec, owned,
            status: Arc::new(Mutex::new(ServerStatus::Stopped)),
            request_tx: Arc::new(Mutex::new(None)),
            listener: Mutex::new(None),
            clients: Arc::new(Mutex::new(HashMap::new())),
            request_map: Arc::new(Mutex::new(HashMap::new())),
            id_allocator: Arc::new(IdAllocator::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(Notify::new()),
            ready_notify: Arc::new(Notify::new()),
            upstream_ready: Arc::new(Notify::new()),
            started_at: Mutex::new(None),
            total_connections: Arc::new(AtomicU32::new(0)),
            cleanup_counter: Arc::new(AtomicU32::new(0)),
            exit_complete_tx: Arc::new(Mutex::new(None)),
            exit_complete_rx: Mutex::new(None),
        }
    }

    pub fn status(&self) -> ServerStatus {
        *self.status.lock()
    }

    pub fn socket_path(&self) -> PathBuf {
        self.socket_path.clone()
    }

    pub fn is_owned(&self) -> bool {
        self.owned
    }

    /// Transport label surfaced to clients via `McpServerStatus`. Derived from
    /// the upstream spec so it reflects how the pool actually talks to the
    /// server (stdio child vs http/sse endpoint).
    pub fn transport(&self) -> &str {
        match &self.spec {
            UpstreamSpec::Stdio { .. } => "stdio",
            UpstreamSpec::Http { sse: false, .. } => "http",
            UpstreamSpec::Http { sse: true, .. } => "sse",
        }
    }

    pub fn uptime_seconds(&self) -> Option<u64> {
        self.started_at
            .lock()
            .map(|start| start.elapsed().as_secs())
    }

    pub fn connection_count(&self) -> u32 {
        self.total_connections.load(Ordering::SeqCst)
    }

    #[allow(dead_code)]
    pub fn ready_notifier(&self) -> Arc<Notify> {
        self.ready_notify.clone()
    }

    pub fn take_exit_receiver(&self) -> Option<oneshot::Receiver<()>> {
        self.exit_complete_rx.lock().take()
    }

    pub fn start(&self) -> io::Result<()> {
        if self.status() == ServerStatus::Running {
            return Ok(());
        }

        // Discovered/external socket: the pool does not own the upstream, so it
        // just marks itself ready and lets clients connect to the pre-existing
        // endpoint.
        if !self.owned {
            *self.status.lock() = ServerStatus::Running;
            self.ready_notify.notify_waiters();
            return Ok(());
        }

        *self.status.lock() = ServerStatus::Starting;
        self.shutdown.store(false, Ordering::SeqCst);

        diagnostics::log(format!("pool_proxy_starting name={} transport={}", self.name, self.transport()));

        let (exit_tx, exit_rx) = oneshot::channel::<()>();
        *self.exit_complete_tx.lock() = Some(exit_tx);
        // Any prior receiver is from a previous run; replace it.
        *self.exit_complete_rx.lock() = Some(exit_rx);

        // Response channel: upstream emits one JSON-RPC object per String here;
        // the router consumes them and dispatches by id.
        let (response_tx, response_rx) = mpsc::channel::<String>(1024);

        // Bind before flipping to Running so ready_notifier waiters connect
        // against a live endpoint.
        let listener = Arc::new(crate::transport::bind(&self.socket_path)?);
        *self.listener.lock() = Some(listener.clone());

        self.spawn_upstream_and_router(response_tx, response_rx);
        self.spawn_accept_loop(listener);

        // Optimistically Running: the upstream task flips us to Stopped on spawn
        // failure or exit, overriding this. Bind success + tasks spawned == ready.
        *self.status.lock() = ServerStatus::Running;
        *self.started_at.lock() = Some(Instant::now());
        self.ready_notify.notify_waiters();

        diagnostics::log(format!("pool_proxy_started name={} socket={}", self.name, self.socket_path.display()));
        Ok(())
    }

    /// Spawn the async upstream bootstrap (`UpstreamHandle::spawn` is async) plus
    /// the response router. On spawn error or upstream exit the proxy is marked
    /// Stopped and shutdown is signaled so the accept loop and waiters tear down.
    fn spawn_upstream_and_router(
        &self,
        response_tx: mpsc::Sender<String>,
        mut response_rx: mpsc::Receiver<String>,
    ) {
        let spec = self.spec.clone();
        let request_tx_slot = self.request_tx.clone();
        let status = self.status.clone();
        let shutdown = self.shutdown.clone();
        let shutdown_notify = self.shutdown_notify.clone();
        let ready_notify = self.ready_notify.clone();
        let upstream_ready = self.upstream_ready.clone();
        let clients = self.clients.clone();
        let request_map = self.request_map.clone();
        let cleanup_counter = self.cleanup_counter.clone();
        let exit_complete_tx = self.exit_complete_tx.clone();
        let name = self.name.clone();

        tokio::spawn(async move {
            // Centralize the stopped-transition side effects (status flip,
            // shutdown signal, exit-complete firing) so spawn-fail and normal
            // exit stay in sync.
            let mark_stopped = || {
                *status.lock() = ServerStatus::Stopped;
                shutdown.store(true, Ordering::SeqCst);
                shutdown_notify.notify_waiters();
                ready_notify.notify_waiters();
                // Wake any client waiting for the upstream sender so it re-checks,
                // sees shutdown, and stops waiting instead of blocking the timeout.
                upstream_ready.notify_waiters();
                if let Some(tx) = exit_complete_tx.lock().take() {
                    let _ = tx.send(());
                }
            };

            let handle = match UpstreamHandle::spawn(spec, response_tx).await {
                Ok(handle) => handle,
                Err(error) => {
                    diagnostics::log(format!("pool_upstream_spawn_failed name={} error={}", name, error));
                    mark_stopped();
                    return;
                }
            };

            *request_tx_slot.lock() = Some(handle.request_tx.clone());
            // Publish readiness: clients parked in acquire_request_sender wake and
            // forward their queued first request now that the sender exists.
            upstream_ready.notify_waiters();

            // Response router: each upstream message is one JSON-RPC object (no
            // trailing newline). Route by id, broadcast id-less.
            let mut processed: u64 = 0;
            while !shutdown.load(Ordering::SeqCst) {
                let message = tokio::select! {
                    msg = response_rx.recv() => match msg {
                        Some(msg) => msg,
                        None => break,
                    },
                    _ = shutdown_notify.notified() => break,
                };
                route_response(&message, &clients, &request_map, &cleanup_counter).await;
                processed += 1;
                // Periodic gauge so a backed-up router is visible without
                // per-message spam. A climbing pending_requests count means
                // responses are not draining as fast as requests arrive.
                if processed % 500 == 0 {
                    let pending = request_map.lock().len();
                    let live = clients.lock().len();
                    diagnostics::log(format!(
                        "pool_router_gauge processed={} pending_requests={} clients={}",
                        processed, pending, live
                    ));
                }
            }

            diagnostics::log(format!("pool_upstream_exited name={}", name));
            mark_stopped();
        });
    }

    fn spawn_accept_loop(&self, listener: Arc<LocalListener>) {
        let clients = self.clients.clone();
        let request_map = self.request_map.clone();
        let request_tx = self.request_tx.clone();
        let id_allocator = self.id_allocator.clone();
        let upstream_ready = self.upstream_ready.clone();
        let shutdown = self.shutdown.clone();
        let shutdown_notify = self.shutdown_notify.clone();
        let name = self.name.clone();
        let total_connections = self.total_connections.clone();
        let cleanup_counter = self.cleanup_counter.clone();

        tokio::spawn(async move {
            let mut counter = 0u64;
            loop {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                match listener.accept().await {
                    Ok(stream) => {
                        let client_id = format!("{}-client-{}", name, counter);
                        counter += 1;
                        total_connections.fetch_add(1, Ordering::SeqCst);

                        let (tx, rx) = mpsc::channel::<String>(128);
                        clients.lock().insert(client_id.clone(), tx);
                        diagnostics::log(format!("pool_client_connected name={} client_id={}", name, client_id));

                        let clients_for_drop = clients.clone();
                        let request_map_for_drop = request_map.clone();
                        let request_tx_for_client = request_tx.clone();
                        let id_allocator_for_client = id_allocator.clone();
                        let upstream_ready_for_client = upstream_ready.clone();
                        let shutdown_for_client = shutdown.clone();
                        let shutdown_notify_for_client = shutdown_notify.clone();
                        let cleanup_counter_for_client = cleanup_counter.clone();
                        let client_id_clone = client_id.clone();

                        tokio::spawn(async move {
                            handle_client(
                                stream,
                                client_id_clone,
                                request_tx_for_client,
                                upstream_ready_for_client,
                                id_allocator_for_client,
                                request_map_for_drop,
                                clients_for_drop,
                                shutdown_for_client,
                                shutdown_notify_for_client,
                                cleanup_counter_for_client,
                                rx,
                            )
                            .await;
                        });
                    }
                    Err(err) => {
                        diagnostics::log(format!("pool_accept_error name={} error={}", name, err));
                        // Back off so a persistent accept error does not busy-loop.
                        sleep(Duration::from_millis(50)).await;
                    }
                }
            }
        });
    }

    pub fn stop(&self) -> io::Result<()> {
        self.shutdown.store(true, Ordering::SeqCst);
        self.shutdown_notify.notify_waiters();

        // Drop the listener so the accept loop unblocks and exits.
        if let Some(listener) = self.listener.lock().take() {
            drop(listener);
        }

        // Closing the request channel ends the upstream's stdin writer and lets
        // the spawned upstream task observe exit. There is no kill handle on
        // UpstreamHandle, so this is best-effort cooperative shutdown.
        let dropped_sender = self.request_tx.lock().take();
        drop(dropped_sender);

        self.clients.lock().clear();
        self.request_map.lock().clear();
        *self.started_at.lock() = None;

        if self.owned {
            #[cfg(unix)]
            {
                // Stale socket file from a crashed run blocks the next bind.
                // Ignore errors: the file may already be gone.
                let _ = std::fs::remove_file(&self.socket_path);
            }
        } else {
            // No background task for discovered sockets; set Stopped here.
            *self.status.lock() = ServerStatus::Stopped;
        }
        Ok(())
    }

    pub async fn restart(&self) -> io::Result<bool> {
        let was_owned_running = self.owned && self.status() == ServerStatus::Running;
        self.stop()?;
        if was_owned_running {
            // Wait for the upstream task to finish so the next bind does not
            // race a dying child holding the socket/port. Best-effort: if the
            // receiver is missing (already taken / never started) we proceed.
            if let Some(exit_rx) = self.take_exit_receiver() {
                let _ = exit_rx.await;
            }
        }
        // Reset shutdown so start()'s accept loop and router can run again.
        self.shutdown.store(false, Ordering::SeqCst);
        self.start()?;
        Ok(true)
    }
}

/// Pump one client connection: read newline-delimited JSON-RPC requests from the
/// client, translate each request id to a pool-unique id, forward to the
/// upstream, and write routed responses back as they arrive on `rx`. The
/// (client_id, original_id) mapping is recorded under the pool id so responses
/// route back to the right client with the id that client expects.
#[allow(clippy::too_many_arguments)]
async fn handle_client(
    stream: LocalStream,
    client_id: String,
    request_tx: Arc<Mutex<Option<mpsc::Sender<String>>>>,
    upstream_ready: Arc<Notify>,
    id_allocator: Arc<IdAllocator>,
    request_map: RequestMap,
    clients: Arc<Mutex<HashMap<String, ClientSender>>>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    cleanup_counter: Arc<AtomicU32>,
    mut rx: mpsc::Receiver<String>,
) {
    diagnostics::log(format!(
        "pool_handle_client_started client_id={}",
        client_id
    ));

    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);
    let mut buffer = String::new();
    let mut parse_failures = 0u32;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        tokio::select! {
            read_result = reader.read_line(&mut buffer) => match read_result {
                Ok(0) => {
                    diagnostics::log(format!("pool_client_disconnected client_id={}", client_id));
                    break;
                }
                Ok(_) => {
                    let line = buffer.trim_end_matches('\n').to_string();
                    buffer.clear();
                    if line.is_empty() {
                        continue;
                    }
                    // Rewrite the client's request id to a pool-unique id so
                    // concurrent clients never collide on the shared upstream; the
                    // original is restored on the matching response. Notifications
                    // (no id) and unparseable lines forward verbatim.
                    let forward_line = match serde_json::from_str::<Value>(&line) {
                        Ok(Value::Object(object)) => {
                            // Clone the original id (ending the borrow) before moving
                            // the object into `with_id`.
                            let original_id = match object.get("id") {
                                Some(id) if !id.is_null() => Some(id.clone()),
                                _ => None,
                            };
                            match original_id {
                                Some(original_id) => {
                                    let pool_id = id_allocator.allocate();
                                    request_map.lock().insert(
                                        pool_id.to_string(),
                                        (client_id.clone(), original_id, Instant::now()),
                                    );
                                    jsonrpc::with_id(object, Value::from(pool_id))
                                }
                                None => line.clone(),
                            }
                        }
                        Ok(_) => line.clone(),
                        Err(_) => {
                            if parse_failures < 3 {
                                // Throttle log spam from a chatty malformed sender.
                                parse_failures += 1;
                                diagnostics::log(format!(
                                    "pool_request_parse_failed client_id={} bytes={}",
                                    client_id, line.len()
                                ));
                            }
                            line.clone()
                        }
                    };

                    // Wait for the upstream sender if it is still starting rather
                    // than dropping this request: the first initialize/tools-list
                    // must survive cold start for tools to appear promptly.
                    let sender = acquire_request_sender(
                        &request_tx,
                        &upstream_ready,
                        &shutdown,
                        &client_id,
                    )
                    .await;
                    if let Some(sender) = sender {
                        if sender.send(forward_line).await.is_err() {
                            diagnostics::log(format!(
                                "pool_request_forward_failed client_id={} reason=upstream_closed",
                                client_id
                            ));
                            break;
                        }
                        diagnostics::log(format!(
                            "pool_request_forwarded client_id={} bytes={}",
                            client_id, line.len()
                        ));
                    } else {
                        diagnostics::log(format!(
                            "pool_request_dropped client_id={} reason=upstream_unavailable",
                            client_id
                        ));
                    }
                }
                Err(err) => {
                    diagnostics::log(format!(
                        "pool_client_read_error client_id={} error={}",
                        client_id, err
                    ));
                    break;
                }
            },
            message = rx.recv() => match message {
                Some(message) => {
                    let mut bytes = message.into_bytes();
                    bytes.push(b'\n');
                    if let Err(err) = write_half.write_all(&bytes).await {
                        diagnostics::log(format!(
                            "pool_client_write_failed client_id={} error={}",
                            client_id, err
                        ));
                        break;
                    }
                    if let Err(err) = write_half.flush().await {
                        diagnostics::log(format!(
                            "pool_client_flush_failed client_id={} error={}",
                            client_id, err
                        ));
                        break;
                    }
                }
                None => break,
            },
            _ = shutdown_notify.notified() => break,
        }
    }

    // Drop this client's in-flight requests so responses are not routed to a
    // (now closed) sender, then remove it from the client table.
    request_map.lock().retain(|_, (cid, _, _)| cid != &client_id);
    clients.lock().remove(&client_id);
    cleanup_stale_requests(&request_map, &cleanup_counter);
}

const UPSTREAM_READY_TIMEOUT_SECS: u64 = 30;

/// Obtain the upstream request sender, waiting if the upstream is still starting.
/// Returns `None` only if the upstream stops or never publishes a sender within
/// the timeout, so a client's first request is queued through cold start instead
/// of being silently dropped.
async fn acquire_request_sender(
    request_tx: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    upstream_ready: &Arc<Notify>,
    shutdown: &Arc<AtomicBool>,
    client_id: &str,
) -> Option<mpsc::Sender<String>> {
    if let Some(sender) = request_tx.lock().clone() {
        return Some(sender);
    }
    diagnostics::log(format!("pool_upstream_wait client_id={}", client_id));
    let deadline = Instant::now() + Duration::from_secs(UPSTREAM_READY_TIMEOUT_SECS);
    loop {
        // Arm the waiter before re-checking the slot so a notify firing between
        // the check and the await is not lost (lost-wakeup safe).
        let ready = upstream_ready.notified();
        tokio::pin!(ready);
        ready.as_mut().enable();

        if let Some(sender) = request_tx.lock().clone() {
            return Some(sender);
        }
        if shutdown.load(Ordering::SeqCst) {
            return None;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return request_tx.lock().clone();
        }
        if tokio::time::timeout(remaining, ready).await.is_err() {
            return request_tx.lock().clone();
        }
    }
}

/// Route one upstream response (single JSON-RPC object, no trailing newline) to
/// the client that issued the matching request, restoring that client's original
/// id. Id-less notifications and ids we never issued (e.g. server-initiated
/// requests) broadcast to all clients; a response whose client has disconnected
/// is dropped.
async fn route_response(
    line: &str,
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    request_map: &RequestMap,
    cleanup_counter: &Arc<AtomicU32>,
) {
    cleanup_stale_requests(request_map, cleanup_counter);

    // Look up the pool id, restore the client's original id, and target that
    // client. Anything without a matching pending request broadcasts.
    let routed: Option<(String, String)> = match serde_json::from_str::<Value>(line) {
        Ok(Value::Object(object)) => {
            // Compute the lookup key (ending the borrow) before moving the object
            // into `with_id`.
            let key = match object.get("id") {
                Some(id) if !id.is_null() => Some(jsonrpc::id_key(id)),
                _ => None,
            };
            match key {
                Some(key) => match request_map.lock().remove(&key) {
                    Some((client_id, original_id, _)) => {
                        Some((client_id, jsonrpc::with_id(object, original_id)))
                    }
                    None => None,
                },
                None => None,
            }
        }
        Ok(_) => None,
        Err(_) => {
            diagnostics::log(format!("pool_response_parse_failed bytes={}", line.len()));
            None
        }
    };

    let Some((client_id, payload)) = routed else {
        broadcast_to_all(line, clients).await;
        return;
    };

    let sender = clients.lock().get(&client_id).cloned();
    let Some(sender) = sender else {
        // The originating client is gone. Dropping is correct here: rebroadcasting
        // a response bearing that client's original id could collide with another
        // client's in-flight id.
        diagnostics::log(format!("pool_response_orphaned client_id={}", client_id));
        return;
    };

    let byte_len = payload.len();
    // The response router is a single task shared by every client. When a client
    // drains its bounded channel slower than responses arrive, `send().await`
    // parks the *whole* router here — head-of-line blocking that stalls every
    // other client. try_send first so the stall is recorded (and timed) instead
    // of happening silently.
    match sender.try_send(payload) {
        Ok(()) => diagnostics::log(format!(
            "pool_response_routed client_id={} bytes={}",
            client_id, byte_len
        )),
        Err(mpsc::error::TrySendError::Full(payload)) => {
            let blocked_since = Instant::now();
            diagnostics::log(format!(
                "pool_router_blocked client_id={} reason=client_channel_full",
                client_id
            ));
            if sender.send(payload).await.is_ok() {
                diagnostics::log(format!(
                    "pool_response_routed client_id={} bytes={} blocked_ms={}",
                    client_id,
                    byte_len,
                    blocked_since.elapsed().as_millis()
                ));
            } else {
                diagnostics::log(format!("pool_response_send_failed client_id={}", client_id));
            }
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            diagnostics::log(format!("pool_response_send_failed client_id={}", client_id));
        }
    }
}

/// Every CLEANUP_INTERVAL-th call, drop request-map entries older than
/// REQUEST_TTL_SECS. Throttled via a counter so the router hot path stays cheap.
fn cleanup_stale_requests(request_map: &RequestMap, cleanup_counter: &Arc<AtomicU32>) {
    let count = cleanup_counter.fetch_add(1, Ordering::Relaxed);
    if count % CLEANUP_INTERVAL != 0 {
        return;
    }

    let now = Instant::now();
    let before = request_map.lock().len();
    request_map.lock().retain(|_, (_, _, inserted_at)| {
        now.duration_since(*inserted_at).as_secs() <= REQUEST_TTL_SECS
    });
    let after = request_map.lock().len();
    // saturating_sub guards against theoretical underflow rather than panicking.
    let removed = before.saturating_sub(after);
    if removed > 0 {
        diagnostics::log(format!("pool_stale_requests_cleaned removed={} remaining={}", removed, after));
    }
}

async fn broadcast_to_all(line: &str, clients: &Arc<Mutex<HashMap<String, ClientSender>>>) {
    let senders: Vec<ClientSender> = clients.lock().values().cloned().collect();
    for sender in &senders {
        let _ = sender.send(line.to_string()).await;
    }
    diagnostics::log(format!("pool_response_broadcast bytes={} clients={}", line.len(), senders.len()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn channel_client(
        clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
        id: &str,
    ) -> mpsc::Receiver<String> {
        let (tx, rx) = mpsc::channel::<String>(8);
        clients.lock().insert(id.to_string(), tx);
        rx
    }

    // The core multiplexing fix: two clients that independently used the same
    // raw id (1) are tracked under distinct pool ids, so their responses route
    // back to the right client with each client's original id restored — no
    // cross-wiring, no broadcast.
    #[tokio::test]
    async fn route_response_restores_ids_without_cross_wiring() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        request_map
            .lock()
            .insert("1".into(), ("clientA".into(), json!(1), Instant::now()));
        request_map
            .lock()
            .insert("2".into(), ("clientB".into(), json!(1), Instant::now()));

        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let mut rx_b = channel_client(&clients, "clientB");
        let counter = Arc::new(AtomicU32::new(0));

        route_response(
            r#"{"jsonrpc":"2.0","id":1,"result":"A"}"#,
            &clients,
            &request_map,
            &counter,
        )
        .await;
        route_response(
            r#"{"jsonrpc":"2.0","id":2,"result":"B"}"#,
            &clients,
            &request_map,
            &counter,
        )
        .await;

        let a: Value = serde_json::from_str(&rx_a.try_recv().expect("clientA response"))
            .expect("valid json");
        let b: Value = serde_json::from_str(&rx_b.try_recv().expect("clientB response"))
            .expect("valid json");
        assert_eq!(a["id"], json!(1), "clientA original id restored");
        assert_eq!(a["result"], json!("A"));
        assert_eq!(b["id"], json!(1), "clientB original id restored");
        assert_eq!(b["result"], json!("B"));

        // Each client received exactly one message: no cross-wiring.
        assert!(rx_a.try_recv().is_err());
        assert!(rx_b.try_recv().is_err());
    }

    // Ids we never issued (server-initiated requests) and id-less notifications
    // fan out to every client.
    #[tokio::test]
    async fn route_response_broadcasts_unmatched_and_notifications() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let mut rx_b = channel_client(&clients, "clientB");
        let counter = Arc::new(AtomicU32::new(0));

        route_response(
            r#"{"jsonrpc":"2.0","id":999,"method":"ping"}"#,
            &clients,
            &request_map,
            &counter,
        )
        .await;
        assert!(rx_a.try_recv().is_ok());
        assert!(rx_b.try_recv().is_ok());

        route_response(
            r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#,
            &clients,
            &request_map,
            &counter,
        )
        .await;
        assert!(rx_a.try_recv().is_ok());
        assert!(rx_b.try_recv().is_ok());
    }

    // A response whose originating client has disconnected is dropped, never
    // rebroadcast (its restored id could collide with a live client's in-flight id).
    #[tokio::test]
    async fn route_response_drops_when_origin_client_gone() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        request_map
            .lock()
            .insert("5".into(), ("ghost".into(), json!(1), Instant::now()));
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_other = channel_client(&clients, "other");
        let counter = Arc::new(AtomicU32::new(0));

        route_response(
            r#"{"jsonrpc":"2.0","id":5,"result":"X"}"#,
            &clients,
            &request_map,
            &counter,
        )
        .await;

        assert!(rx_other.try_recv().is_err(), "orphan response not broadcast");
    }
}
