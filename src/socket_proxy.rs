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
use crate::mcp_session::{
    ClientCapabilities, HandshakeCache, PendingRequestInfo, PendingWaiter, RecoveryReason,
    build_error_response, build_success_response, cacheable_method, is_empty_id_error,
    is_session_not_found_error, parse_client_capabilities, should_swallow_initialized,
};
use crate::transport::{LocalListener, LocalStream};
use crate::types::ServerStatus;
use crate::upstream::{UpstreamHandle, UpstreamSpec};

// Throttle/expire the pending-request map opportunistically rather than on a timer.
const REQUEST_TTL_SECS: u64 = 300;
const CLEANUP_INTERVAL: u32 = 100;

type ClientSender = mpsc::Sender<String>;

type RequestMap = Arc<Mutex<HashMap<String, PendingRequestInfo>>>;
type HandshakeCacheRef = Arc<Mutex<HandshakeCache>>;

/// True for the handshake/discovery methods whose success responses are cached.
fn is_cacheable_method(method: &str) -> bool {
    cacheable_method(method).is_some()
}

/// Cache an upstream response for a cacheable method, but ONLY when it is a
/// success: it must carry a `result` and no `error`. Errors (429, auth failure,
/// -32001, etc.) are never cached so a transient failure is not served to every
/// later client.
fn maybe_cache_response(value: &Value, method: &str, cache: &HandshakeCacheRef) {
    if !is_cacheable_method(method) {
        return;
    }
    if value.get("error").is_some() {
        return;
    }
    let Some(result) = value.get("result") else {
        return;
    };
    cache.lock().store(method, result.clone());
    diagnostics::log(format!("pool_cache_stored method={}", method));
}

fn prepare_tools_list_request(
    cache: &HandshakeCacheRef,
    client_id: &str,
    original_id: Value,
) -> ToolsListAction {
    let mut cache = cache.lock();
    if let Some(result) = cache.tools_list.cached_result.clone() {
        return ToolsListAction::Cached(build_success_response(original_id, result));
    }
    if cache.tools_list.in_flight {
        cache.tools_list.waiters.push(PendingWaiter {
            client_id: client_id.to_string(),
            original_id,
            inserted_at: Instant::now(),
        });
        return ToolsListAction::Coalesced;
    }
    cache.tools_list.in_flight = true;
    ToolsListAction::Leader
}

/// What to do with a parsed client line: forward it upstream, or reply directly
/// from cache without an upstream round-trip.
enum ClientAction {
    Forward {
        line: String,
        method: Option<String>,
        pool_id: Option<u64>,
    },
    Cached(String),
    Drop,
}

enum ToolsListAction {
    Cached(String),
    Coalesced,
    Leader,
}

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
    // Per-upstream cache of successful `initialize` / `tools/list` responses, so
    // a new client served from proxy-side state never re-triggers upstream
    // discovery (and the auth/429 churn that follows).
    handshake_cache: HandshakeCacheRef,
    client_capabilities: Arc<Mutex<HashMap<String, ClientCapabilities>>>,
    // Most-recently-active client: the last client to send a REQUEST. Used as a
    // HEURISTIC to route server-initiated requests (e.g. sampling/createMessage,
    // roots/list) back to a single client, since broadcasting a request that
    // expects one answer would make every client respond. parking_lot::Mutex
    // matches this struct's interior-mutability style.
    last_active_client: Arc<Mutex<Option<String>>>,
    // Per-upstream id translation: client request ids are rewritten to pool-unique
    // ids before forwarding, so concurrent clients (which independently reuse
    // 1, 2, 3, ...) never collide on the shared upstream connection.
    id_allocator: Arc<IdAllocator>,
    shutdown: Arc<AtomicBool>,
    shutdown_notify: Arc<Notify>,
    recovery_requested: Arc<AtomicBool>,
    recovery_tx: mpsc::Sender<RecoveryReason>,
    recovery_rx: Mutex<Option<mpsc::Receiver<RecoveryReason>>>,
    // Fires when the upstream request sender is published (or when the upstream
    // stops), so a client that connects mid-startup waits for the sender instead
    // of dropping its first request.
    upstream_ready: Arc<Notify>,
    started_at: Mutex<Option<Instant>>,
    cleanup_counter: Arc<AtomicU32>,
    // Fired when the upstream task exits, so `restart` can await a clean stop.
    exit_complete_tx: Arc<Mutex<Option<oneshot::Sender<()>>>>,
    exit_complete_rx: Mutex<Option<oneshot::Receiver<()>>>,
}

impl SocketProxy {
    pub fn new(name: String, socket_path: PathBuf, spec: UpstreamSpec, owned: bool) -> Self {
        let (recovery_tx, recovery_rx) = mpsc::channel::<RecoveryReason>(8);
        Self {
            name,
            socket_path,
            spec,
            owned,
            status: Arc::new(Mutex::new(ServerStatus::Stopped)),
            request_tx: Arc::new(Mutex::new(None)),
            listener: Mutex::new(None),
            clients: Arc::new(Mutex::new(HashMap::new())),
            request_map: Arc::new(Mutex::new(HashMap::new())),
            handshake_cache: Arc::new(Mutex::new(HandshakeCache::default())),
            client_capabilities: Arc::new(Mutex::new(HashMap::new())),
            last_active_client: Arc::new(Mutex::new(None)),
            id_allocator: Arc::new(IdAllocator::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(Notify::new()),
            recovery_requested: Arc::new(AtomicBool::new(false)),
            recovery_tx,
            recovery_rx: Mutex::new(Some(recovery_rx)),
            upstream_ready: Arc::new(Notify::new()),
            started_at: Mutex::new(None),
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
        // Current active clients, not a cumulative total: a counter that only ever
        // increments reports phantom connections (every past client plus each
        // liveness probe from `socket_alive`). The `clients` map is the live set —
        // entries are inserted on accept and removed when the client disconnects.
        self.clients.lock().len() as u32
    }

    pub fn take_exit_receiver(&self) -> Option<oneshot::Receiver<()>> {
        self.exit_complete_rx.lock().take()
    }

    pub fn start(self: &Arc<Self>) -> io::Result<()> {
        if self.status() == ServerStatus::Running {
            return Ok(());
        }

        // Discovered/external socket: the pool does not own the upstream, so it
        // just marks itself ready and lets clients connect to the pre-existing
        // endpoint.
        if !self.owned {
            *self.status.lock() = ServerStatus::Running;
            return Ok(());
        }

        *self.status.lock() = ServerStatus::Starting;
        self.shutdown.store(false, Ordering::SeqCst);

        diagnostics::log(format!(
            "pool_proxy_starting name={} transport={}",
            self.name,
            self.transport()
        ));

        let (exit_tx, exit_rx) = oneshot::channel::<()>();
        *self.exit_complete_tx.lock() = Some(exit_tx);
        // Any prior receiver is from a previous run; replace it.
        *self.exit_complete_rx.lock() = Some(exit_rx);

        // Response channel: upstream emits one JSON-RPC object per String here;
        // the router consumes them and dispatches by id.
        let (response_tx, response_rx) = mpsc::channel::<String>(1024);

        // Bind before flipping to Running so a client connecting the instant we
        // report Running reaches a live endpoint.
        let listener = Arc::new(crate::transport::bind(&self.socket_path)?);
        *self.listener.lock() = Some(listener.clone());

        self.spawn_upstream_and_router(response_tx, response_rx);
        self.spawn_accept_loop(listener);
        self.spawn_recovery_loop();

        // Optimistically Running: the upstream task flips us to Stopped on spawn
        // failure or exit, overriding this. Bind success + tasks spawned == ready.
        *self.status.lock() = ServerStatus::Running;
        *self.started_at.lock() = Some(Instant::now());

        diagnostics::log(format!(
            "pool_proxy_started name={} socket={}",
            self.name,
            self.socket_path.display()
        ));
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
        let upstream_ready = self.upstream_ready.clone();
        let clients = self.clients.clone();
        let request_map = self.request_map.clone();
        let request_tx = self.request_tx.clone();
        let handshake_cache = self.handshake_cache.clone();
        let client_capabilities = self.client_capabilities.clone();
        let last_active_client = self.last_active_client.clone();
        let cleanup_counter = self.cleanup_counter.clone();
        let recovery_tx = self.recovery_tx.clone();
        let recovery_requested = self.recovery_requested.clone();
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
                    diagnostics::log(format!(
                        "pool_upstream_spawn_failed name={} error={}",
                        name, error
                    ));
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
                route_response(
                    &message,
                    &clients,
                    &request_map,
                    &handshake_cache,
                    &cleanup_counter,
                    &last_active_client,
                    &client_capabilities,
                    &request_tx,
                    &recovery_tx,
                    &recovery_requested,
                )
                .await;
                processed += 1;
                // Periodic gauge so a backed-up router is visible without
                // per-message spam. A climbing pending_requests count means
                // responses are not draining as fast as requests arrive.
                if processed.is_multiple_of(500) {
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
        let handshake_cache = self.handshake_cache.clone();
        let client_capabilities = self.client_capabilities.clone();
        let last_active_client = self.last_active_client.clone();
        let request_tx = self.request_tx.clone();
        let id_allocator = self.id_allocator.clone();
        let upstream_ready = self.upstream_ready.clone();
        let shutdown = self.shutdown.clone();
        let shutdown_notify = self.shutdown_notify.clone();
        let name = self.name.clone();
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

                        let (tx, rx) = mpsc::channel::<String>(128);
                        clients.lock().insert(client_id.clone(), tx);
                        diagnostics::log(format!(
                            "pool_client_connected name={} client_id={}",
                            name, client_id
                        ));

                        let clients_for_drop = clients.clone();
                        let request_map_for_drop = request_map.clone();
                        let handshake_cache_for_client = handshake_cache.clone();
                        let client_capabilities_for_client = client_capabilities.clone();
                        let last_active_for_client = last_active_client.clone();
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
                                handshake_cache_for_client,
                                client_capabilities_for_client,
                                last_active_for_client,
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
        // Reset the handshake cache so a restarted upstream (potentially a new
        // process with different tools) is rediscovered, not served stale.
        *self.handshake_cache.lock() = HandshakeCache::default();
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

    pub async fn restart(self: &Arc<Self>) -> io::Result<bool> {
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
        let mut last_error = None;
        for attempt in 1..=5 {
            match self.start() {
                Ok(()) => return Ok(true),
                Err(error) => {
                    diagnostics::log(format!(
                        "pool_restart_start_retry name={} attempt={} error={}",
                        self.name, attempt, error
                    ));
                    last_error = Some(error);
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
        *self.status.lock() = ServerStatus::Stopped;
        match last_error {
            Some(error) => Err(error),
            None => Err(io::Error::other("restart failed without error")),
        }
    }

    fn spawn_recovery_loop(self: &Arc<Self>) {
        let Some(mut recovery_rx) = self.recovery_rx.lock().take() else {
            return;
        };
        let proxy = self.clone();
        tokio::spawn(async move {
            while let Some(reason) = recovery_rx.recv().await {
                if proxy.recovery_requested.swap(true, Ordering::SeqCst) {
                    continue;
                }
                diagnostics::log(format!(
                    "pool_recovery_start name={} reason={}",
                    proxy.name,
                    recovery_reason_label(reason)
                ));
                let result = proxy.restart().await;
                match result {
                    Ok(true) => diagnostics::log(format!("pool_recovery_done name={}", proxy.name)),
                    Ok(false) => diagnostics::log(format!(
                        "pool_recovery_failed name={} reason=not_owned_or_not_running",
                        proxy.name
                    )),
                    Err(error) => diagnostics::log(format!(
                        "pool_recovery_failed name={} error={}",
                        proxy.name, error
                    )),
                }
                proxy.recovery_requested.store(false, Ordering::SeqCst);
            }
        });
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
    handshake_cache: HandshakeCacheRef,
    client_capabilities: Arc<Mutex<HashMap<String, ClientCapabilities>>>,
    last_active_client: Arc<Mutex<Option<String>>>,
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
    let mut locally_initialized = false;

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
                    // Classify by JSON-RPC message kind (presence of `method`
                    // and a non-null `id`) rather than by id alone. Only a
                    // REQUEST gets its id rewritten and stored; a client
                    // RESPONSE (no `method`) answers a server-initiated request
                    // and carries the SERVER's id, so it must pass through with
                    // its id intact and unstored. Notifications and unparseable
                    // lines forward verbatim. A cacheable handshake REQUEST whose
                    // success response is already cached is answered directly,
                    // skipping the upstream entirely.
                    let action = match serde_json::from_str::<Value>(&line) {
                        Ok(value) if value.is_object() => {
                            // Clone the original id (ending the borrow) before
                            // moving the object into `with_id`.
                            let original_id = non_null_id(&value).cloned();
                            match (message_has_method(&value), original_id) {
                                // REQUEST: method + non-null id.
                                (true, Some(original_id)) => {
                                    let method = value
                                        .get("method")
                                        .and_then(Value::as_str)
                                        .map(str::to_string);
                                    diagnostics::log(format!(
                                        "pool_request_received client_id={} method={} has_id=true bytes={}",
                                        client_id,
                                        method.as_deref().unwrap_or("?"),
                                        line.len()
                                    ));
                                    if method.as_deref() == Some("initialize") {
                                        client_capabilities.lock().insert(
                                            client_id.clone(),
                                            parse_client_capabilities(&value),
                                        );
                                    }
                                    match method.as_deref() {
                                        Some("initialize") => {
                                            let cached = handshake_cache.lock().get("initialize");
                                            if let Some(result) = cached {
                                                diagnostics::log(format!(
                                                    "pool_cache_hit method=initialize client_id={}",
                                                    client_id
                                                ));
                                                locally_initialized = true;
                                                ClientAction::Cached(build_success_response(original_id, result))
                                            } else {
                                                diagnostics::log(format!(
                                                    "pool_cache_miss method=initialize client_id={}",
                                                    client_id
                                                ));
                                                let pool_id = id_allocator.allocate();
                                                request_map.lock().insert(
                                                    jsonrpc::id_key(&Value::from(pool_id)),
                                                    PendingRequestInfo {
                                                        client_id: client_id.clone(),
                                                        original_id,
                                                        method: Some("initialize".to_string()),
                                                        inserted_at: Instant::now(),
                                                    },
                                                );
                                                *last_active_client.lock() = Some(client_id.clone());
                                                match value.clone() {
                                                    Value::Object(object) => ClientAction::Forward {
                                                        line: jsonrpc::with_id(object, Value::from(pool_id)),
                                                        method: Some("initialize".to_string()),
                                                        pool_id: Some(pool_id),
                                                    },
                                                    _ => ClientAction::Forward {
                                                        line: line.clone(),
                                                        method: Some("initialize".to_string()),
                                                        pool_id: Some(pool_id),
                                                    },
                                                }
                                            }
                                        }
                                        Some("tools/list") => {
                                            match prepare_tools_list_request(
                                                &handshake_cache,
                                                &client_id,
                                                original_id.clone(),
                                            ) {
                                                ToolsListAction::Cached(response) => {
                                                    diagnostics::log(format!(
                                                        "pool_cache_hit method=tools/list client_id={}",
                                                        client_id
                                                    ));
                                                    ClientAction::Cached(response)
                                                }
                                                ToolsListAction::Coalesced => {
                                                    diagnostics::log(format!(
                                                        "pool_tools_list_coalesced client_id={}",
                                                        client_id
                                                    ));
                                                    ClientAction::Drop
                                                }
                                                ToolsListAction::Leader => {
                                                diagnostics::log(format!(
                                                    "pool_cache_miss method=tools/list client_id={}",
                                                    client_id
                                                ));
                                                let pool_id = id_allocator.allocate();
                                                request_map.lock().insert(
                                                    jsonrpc::id_key(&Value::from(pool_id)),
                                                    PendingRequestInfo {
                                                        client_id: client_id.clone(),
                                                        original_id,
                                                        method: Some("tools/list".to_string()),
                                                        inserted_at: Instant::now(),
                                                    },
                                                );
                                                *last_active_client.lock() = Some(client_id.clone());
                                                match value.clone() {
                                                    Value::Object(object) => ClientAction::Forward {
                                                        line: jsonrpc::with_id(object, Value::from(pool_id)),
                                                        method: Some("tools/list".to_string()),
                                                        pool_id: Some(pool_id),
                                                    },
                                                    _ => ClientAction::Forward {
                                                        line: line.clone(),
                                                        method: Some("tools/list".to_string()),
                                                        pool_id: Some(pool_id),
                                                    },
                                                }
                                                }
                                            }
                                        }
                                        _ => {
                                        let pool_id = id_allocator.allocate();
                                        // Remember the method only for cacheable
                                        // ones, so route_response can cache the
                                        // matching success response.
                                        let forward_method = method.clone();
                                        let cacheable = method
                                            .filter(|m| is_cacheable_method(m));
                                        // Key the pending request through the same
                                        // canonical helper route_response uses to
                                        // look it up, so the insert and lookup keys
                                        // cannot drift (numeric id -> identical
                                        // string).
                                        request_map.lock().insert(
                                            jsonrpc::id_key(&Value::from(pool_id)),
                                            PendingRequestInfo {
                                                client_id: client_id.clone(),
                                                original_id,
                                                method: cacheable,
                                                inserted_at: Instant::now(),
                                            },
                                        );
                                        // Record this client as most-recently-active
                                        // so a server-initiated callback can route
                                        // back to it (see route_server_request).
                                        *last_active_client.lock() = Some(client_id.clone());
                                        match value.clone() {
                                            Value::Object(object) => ClientAction::Forward {
                                                line: jsonrpc::with_id(object, Value::from(pool_id)),
                                                method: forward_method,
                                                pool_id: Some(pool_id),
                                            },
                                            // Unreachable: guarded by is_object
                                            // above, but match instead of unwrap to
                                            // stay panic-free.
                                            _ => ClientAction::Forward {
                                                line: line.clone(),
                                                method: forward_method,
                                                pool_id: Some(pool_id),
                                            },
                                        }
                                        }
                                    }
                                }
                                // NOTIFICATION (method, no id) or RESPONSE
                                // (no method): forward verbatim, never store.
                                _ => {
                                    if should_swallow_initialized(locally_initialized, &value) {
                                        diagnostics::log(format!(
                                            "pool_cached_initialized_swallowed client_id={}",
                                            client_id
                                        ));
                                        ClientAction::Drop
                                    } else {
                                        let method = value
                                            .get("method")
                                            .and_then(Value::as_str)
                                            .map(str::to_string);
                                        diagnostics::log(format!(
                                            "pool_request_received client_id={} method={} has_id=false bytes={}",
                                            client_id,
                                            method.as_deref().unwrap_or("?"),
                                            line.len()
                                        ));
                                        ClientAction::Forward {
                                            line: line.clone(),
                                            method,
                                            pool_id: None,
                                        }
                                    }
                                }
                            }
                        }
                        Ok(_) => ClientAction::Forward {
                            line: line.clone(),
                            method: None,
                            pool_id: None,
                        },
                        Err(_) => {
                            if parse_failures < 3 {
                                // Throttle log spam from a chatty malformed sender.
                                parse_failures += 1;
                                diagnostics::log(format!(
                                    "pool_request_parse_failed client_id={} bytes={}",
                                    client_id, line.len()
                                ));
                            }
                            ClientAction::Forward {
                                line: line.clone(),
                                method: None,
                                pool_id: None,
                            }
                        }
                    };

                    let (forward_line, forward_method, forward_pool_id) = match action {
                        // Cache hit: reply directly to this client. handle_client
                        // owns write_half and the select! arms never run
                        // concurrently, so writing here is not re-entrant.
                        ClientAction::Cached(response) => {
                            let mut bytes = response.into_bytes();
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
                            continue;
                        }
                        ClientAction::Drop => continue,
                        ClientAction::Forward {
                            line,
                            method,
                            pool_id,
                        } => (line, method, pool_id),
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
                            "pool_request_forwarded client_id={} method={} pool_id={} bytes={}",
                            client_id,
                            forward_method.as_deref().unwrap_or("?"),
                            forward_pool_id
                                .map(|pool_id| pool_id.to_string())
                                .unwrap_or_else(|| "?".to_string()),
                            line.len()
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
    request_map
        .lock()
        .retain(|_, pending| pending.client_id != client_id);
    clients.lock().remove(&client_id);
    client_capabilities.lock().remove(&client_id);
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

/// Route one upstream message (single JSON-RPC object, no trailing newline) to
/// the right client(s), classified by JSON-RPC message kind:
/// - RESPONSE (no `method`, has id): restore the issuing client's original id
///   and send to that one client; drop if the id was never issued or the client
///   has disconnected (never rebroadcast — a restored id could collide with a
///   live client's in-flight id).
/// - SERVER-INITIATED REQUEST (`method` + id the pool never issued): route to a
///   single client (broadcasting would make every client answer the same
///   request, producing duplicate/conflicting responses upstream).
/// - NOTIFICATION (`method`, no id): broadcast to every client.
/// - unparseable / non-object: broadcast (avoid silently dropping data).
#[allow(clippy::too_many_arguments)]
async fn route_response(
    line: &str,
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    request_map: &RequestMap,
    handshake_cache: &HandshakeCacheRef,
    cleanup_counter: &Arc<AtomicU32>,
    last_active_client: &Arc<Mutex<Option<String>>>,
    client_capabilities: &Arc<Mutex<HashMap<String, ClientCapabilities>>>,
    request_tx: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    recovery_tx: &mpsc::Sender<RecoveryReason>,
    recovery_requested: &Arc<AtomicBool>,
) {
    let stale_requests = cleanup_stale_requests(request_map, cleanup_counter);
    for (client_id, payload) in
        cleanup_tools_list_after_stale_requests(handshake_cache, &stale_requests)
    {
        send_to_client(&client_id, payload, clients).await;
    }
    for (client_id, payload) in cleanup_stale_tools_list_waiters(handshake_cache) {
        send_to_client(&client_id, payload, clients).await;
    }

    match serde_json::from_str::<Value>(line) {
        Ok(value) if value.is_object() => {
            request_recovery_if_session_not_found(
                &value,
                handshake_cache,
                recovery_tx,
                recovery_requested,
            );
            // Clone the id (ending the borrow) before potentially moving the
            // object into `with_id`.
            let id = non_null_id(&value).cloned();
            match (message_has_method(&value), id) {
                // SERVER-INITIATED REQUEST: method + id. Route to one client.
                (true, Some(_)) => {
                    route_server_request(
                        line,
                        &value,
                        clients,
                        last_active_client,
                        client_capabilities,
                        request_tx,
                    )
                    .await;
                }
                // RESPONSE: no method, has id. Restore the client's original id.
                (false, Some(id)) => {
                    let key = jsonrpc::id_key(&id);
                    let restored = match request_map.lock().remove(&key) {
                        Some(pending) => {
                            if pending.method.as_deref() == Some("tools/list") {
                                complete_tools_list_response(&value, pending, handshake_cache)
                            } else {
                                diagnostics::log(format!(
                                    "pool_response_routed client_id={} method={} elapsed_ms={} outcome={}",
                                    pending.client_id,
                                    pending.method.as_deref().unwrap_or("?"),
                                    pending.inserted_at.elapsed().as_millis(),
                                    response_outcome(&value)
                                ));
                                // Cache successful handshake/discovery responses
                                // (result, no error) so later clients are served from
                                // proxy-side state. Errors are never cached.
                                if let Some(method) = pending.method.as_deref() {
                                    maybe_cache_response(&value, method, handshake_cache);
                                }
                                match value.clone() {
                                    Value::Object(object) => {
                                        vec![(
                                            pending.client_id,
                                            jsonrpc::with_id(object, pending.original_id),
                                        )]
                                    }
                                    // Unreachable: guarded by is_object above.
                                    _ => Vec::new(),
                                }
                            }
                        }
                        None => Vec::new(),
                    };
                    if restored.is_empty() {
                        if let Some((client_id, payload, method)) =
                            restore_empty_id_error_to_oldest_pending(&value, request_map)
                        {
                            let (error_code, error_message) = error_summary(&value);
                            diagnostics::log(format!(
                                "pool_response_empty_id_error_routed client_id={} method={} error_code={} error_message={}",
                                client_id,
                                method.as_deref().unwrap_or("?"),
                                error_code,
                                error_message
                            ));
                            send_to_client(&client_id, payload, clients).await;
                        } else {
                            // Orphan/late response: the request is gone (TTL
                            // cleanup or client disconnect). Drop, never broadcast.
                            diagnostics::log(format!(
                                "pool_response_orphaned id={} reason=no_pending_request",
                                key
                            ));
                        }
                    } else {
                        for (client_id, payload) in restored {
                            send_to_client(&client_id, payload, clients).await;
                        }
                    }
                }
                // NOTIFICATION (method, no id) or non-routable object: broadcast.
                _ => {
                    // An upstream tools/list_changed notification invalidates the
                    // tools/list cache (only tools/list, not initialize) before it
                    // is broadcast, so the next tools/list request rediscovers.
                    if value.get("method").and_then(Value::as_str)
                        == Some("notifications/tools/list_changed")
                    {
                        handshake_cache.lock().invalidate_tools_list();
                        diagnostics::log("pool_tools_cache_invalidated");
                    }
                    broadcast_to_all(line, clients).await;
                }
            }
        }
        Ok(_) => broadcast_to_all(line, clients).await,
        Err(_) => {
            diagnostics::log(format!("pool_response_parse_failed bytes={}", line.len()));
            broadcast_to_all(line, clients).await;
        }
    }
}

/// Some upstream bridges (notably Agency's HTTP/SSE bridge when a remote MCP
/// session expires) return an error response with `id:""` instead of echoing the
/// JSON-RPC request id. If we drop that as an orphan, the downstream client waits
/// until its own timeout and the tool appears to hang. Route only these empty-id
/// *errors* to the oldest pending request for this server so the client receives
/// a concrete MCP error. Successful or non-empty-id responses still require exact
/// id matching.
fn restore_empty_id_error_to_oldest_pending(
    value: &Value,
    request_map: &RequestMap,
) -> Option<(String, String, Option<String>)> {
    if !is_empty_id_error(value) {
        return None;
    }

    let mut pending = request_map.lock();
    let key = pending
        .iter()
        .min_by(|(_, left), (_, right)| left.inserted_at.cmp(&right.inserted_at))
        .map(|(key, _)| key.clone())?;
    let request = pending.remove(&key)?;
    match value.clone() {
        Value::Object(object) => Some((
            request.client_id,
            jsonrpc::with_id(object, request.original_id),
            request.method,
        )),
        _ => None,
    }
}

fn complete_tools_list_response(
    value: &Value,
    leader: PendingRequestInfo,
    cache: &HandshakeCacheRef,
) -> Vec<(String, String)> {
    let mut cache = cache.lock();
    cache.tools_list.in_flight = false;
    let waiters: Vec<PendingWaiter> = cache.tools_list.waiters.drain(..).collect();

    if value.get("error").is_none()
        && let Some(result) = value.get("result").cloned()
    {
        cache.tools_list.cached_result = Some(result.clone());
        cache.tools_list.last_good_result = Some(result);
        diagnostics::log("pool_cache_stored method=tools/list");
    }

    let mut responses = Vec::with_capacity(waiters.len() + 1);
    if let Value::Object(object) = value.clone() {
        diagnostics::log(format!(
            "pool_response_routed client_id={} method=tools/list elapsed_ms={} outcome={} waiters={}",
            leader.client_id,
            leader.inserted_at.elapsed().as_millis(),
            response_outcome(value),
            waiters.len()
        ));
        responses.push((
            leader.client_id,
            jsonrpc::with_id(object.clone(), leader.original_id),
        ));
        responses.extend(waiters.into_iter().map(|waiter| {
            (
                waiter.client_id,
                jsonrpc::with_id(object.clone(), waiter.original_id),
            )
        }));
    }
    responses
}

fn cleanup_stale_tools_list_waiters(cache: &HandshakeCacheRef) -> Vec<(String, String)> {
    let now = Instant::now();
    let mut cache = cache.lock();
    let mut stale = Vec::new();
    let mut kept = Vec::with_capacity(cache.tools_list.waiters.len());
    for waiter in cache.tools_list.waiters.drain(..) {
        if now.duration_since(waiter.inserted_at).as_secs() > REQUEST_TTL_SECS {
            stale.push((
                waiter.client_id,
                build_error_response(waiter.original_id, -32001, "tools/list discovery timed out"),
            ));
        } else {
            kept.push(waiter);
        }
    }
    cache.tools_list.waiters = kept;
    stale
}

fn cleanup_tools_list_after_stale_requests(
    cache: &HandshakeCacheRef,
    stale_requests: &[PendingRequestInfo],
) -> Vec<(String, String)> {
    if !stale_requests
        .iter()
        .any(|pending| pending.method.as_deref() == Some("tools/list"))
    {
        return Vec::new();
    }

    let mut cache = cache.lock();
    cache.tools_list.in_flight = false;
    let waiters: Vec<PendingWaiter> = cache.tools_list.waiters.drain(..).collect();
    if !waiters.is_empty() {
        diagnostics::log(format!(
            "pool_tools_list_stale_leader_cleaned waiters={}",
            waiters.len()
        ));
    }

    waiters
        .into_iter()
        .map(|waiter| {
            (
                waiter.client_id,
                build_error_response(waiter.original_id, -32001, "tools/list discovery timed out"),
            )
        })
        .collect()
}

fn request_recovery_if_session_not_found(
    value: &Value,
    cache: &HandshakeCacheRef,
    recovery_tx: &mpsc::Sender<RecoveryReason>,
    recovery_requested: &Arc<AtomicBool>,
) {
    if !is_session_not_found_error(value) {
        return;
    }
    cache.lock().clear_all();
    if recovery_requested.load(Ordering::SeqCst) {
        return;
    }
    if recovery_tx
        .try_send(RecoveryReason::SessionNotFound)
        .is_err()
    {
        diagnostics::log("pool_recovery_signal_failed reason=channel_unavailable");
    }
}

fn response_outcome(value: &Value) -> &'static str {
    if value.get("error").is_some() {
        "error"
    } else {
        "result"
    }
}

fn error_summary(value: &Value) -> (String, String) {
    let Some(error) = value.get("error") else {
        return ("?".to_string(), "?".to_string());
    };
    let code = error
        .get("code")
        .map(Value::to_string)
        .unwrap_or_else(|| "?".to_string());
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    (code, message)
}

fn recovery_reason_label(reason: RecoveryReason) -> &'static str {
    match reason {
        RecoveryReason::SessionNotFound => "session_not_found",
    }
}

/// Route a server-initiated request to a SINGLE client.
///
/// HEURISTIC: target the most-recently-active client — the one currently driving
/// the upstream is assumed to have triggered the callback (e.g. sampling/roots).
/// This is not capability-aware; matching the client that advertised the relevant
/// capability at `initialize` is a future improvement. Falls back to any one
/// connected client if the last-active client has disconnected, and drops the
/// request if there are no clients. The line is forwarded VERBATIM so the
/// server's id is preserved and the client's response id matches.
async fn route_server_request(
    line: &str,
    value: &Value,
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    last_active_client: &Arc<Mutex<Option<String>>>,
    client_capabilities: &Arc<Mutex<HashMap<String, ClientCapabilities>>>,
    request_tx: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
) {
    let method = value.get("method").and_then(Value::as_str).unwrap_or("?");
    if let Some(required) = required_capability(method) {
        let target = capable_client(clients, client_capabilities, last_active_client, required);
        if let Some(client_id) = target {
            diagnostics::log(format!(
                "pool_server_request_routed client_id={} method={}",
                client_id, method
            ));
            send_to_client(&client_id, line.to_string(), clients).await;
            return;
        }

        let Some(server_id) = non_null_id(value).cloned() else {
            diagnostics::log(format!(
                "pool_server_request_dropped method={} reason=no_capable_client",
                method
            ));
            return;
        };
        let response = build_error_response(
            server_id,
            -32001,
            &format!("no capable downstream client connected for {method}"),
        );
        send_to_upstream(response, request_tx, method).await;
        return;
    }

    diagnostics::log(format!("pool_server_request_fallback method={}", method));
    // Snapshot last-active (releasing its lock) before locking clients, so the
    // two parking_lot mutexes are never held nested.
    let last_active = last_active_client.lock().clone();
    let target = {
        let clients_guard = clients.lock();
        match last_active {
            Some(id) if clients_guard.contains_key(&id) => Some(id),
            // Fall back to any one connected client (first by iteration order).
            _ => clients_guard.keys().next().cloned(),
        }
    };

    let Some(client_id) = target else {
        diagnostics::log(format!(
            "pool_server_request_dropped bytes={} reason=no_clients",
            line.len()
        ));
        return;
    };
    diagnostics::log(format!(
        "pool_server_request_routed client_id={}",
        client_id
    ));
    send_to_client(&client_id, line.to_string(), clients).await;
}

#[derive(Debug, Clone, Copy)]
enum RequiredCapability {
    Sampling,
    Roots,
}

fn required_capability(method: &str) -> Option<RequiredCapability> {
    if method.starts_with("sampling/") {
        Some(RequiredCapability::Sampling)
    } else if method.starts_with("roots/") {
        Some(RequiredCapability::Roots)
    } else {
        None
    }
}

fn capable_client(
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    client_capabilities: &Arc<Mutex<HashMap<String, ClientCapabilities>>>,
    last_active_client: &Arc<Mutex<Option<String>>>,
    required: RequiredCapability,
) -> Option<String> {
    let last_active = last_active_client.lock().clone();
    let clients_guard = clients.lock();
    let capabilities_guard = client_capabilities.lock();
    if let Some(client_id) = last_active
        && clients_guard.contains_key(&client_id)
        && capabilities_guard
            .get(&client_id)
            .is_some_and(|capabilities| has_required_capability(capabilities, required))
    {
        return Some(client_id);
    }
    capabilities_guard
        .iter()
        .find_map(|(client_id, capabilities)| {
            if clients_guard.contains_key(client_id)
                && has_required_capability(capabilities, required)
            {
                Some(client_id.clone())
            } else {
                None
            }
        })
}

fn has_required_capability(
    capabilities: &ClientCapabilities,
    required: RequiredCapability,
) -> bool {
    match required {
        RequiredCapability::Sampling => capabilities.sampling,
        RequiredCapability::Roots => capabilities.roots,
    }
}

async fn send_to_upstream(
    payload: String,
    request_tx: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    method: &str,
) {
    let sender = request_tx.lock().clone();
    let Some(sender) = sender else {
        diagnostics::log(format!(
            "pool_server_request_error_send_failed method={} reason=upstream_unavailable",
            method
        ));
        return;
    };
    if sender.send(payload).await.is_err() {
        diagnostics::log(format!(
            "pool_server_request_error_send_failed method={} reason=upstream_closed",
            method
        ));
    }
}

/// Deliver one already-serialized payload to a single client. Preserves the
/// head-of-line-blocking-aware try_send-then-send pattern: the response router is
/// a single task shared by every client, so when a client drains its bounded
/// channel slower than messages arrive, `send().await` parks the *whole* router.
/// try_send first records (and times) the stall instead of stalling silently. A
/// payload for a vanished client is dropped, never rebroadcast.
async fn send_to_client(
    client_id: &str,
    payload: String,
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
) {
    let sender = clients.lock().get(client_id).cloned();
    let Some(sender) = sender else {
        diagnostics::log(format!("pool_response_orphaned client_id={}", client_id));
        return;
    };

    let byte_len = payload.len();
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
fn cleanup_stale_requests(
    request_map: &RequestMap,
    cleanup_counter: &Arc<AtomicU32>,
) -> Vec<PendingRequestInfo> {
    let count = cleanup_counter.fetch_add(1, Ordering::Relaxed);
    if !count.is_multiple_of(CLEANUP_INTERVAL) {
        return Vec::new();
    }

    let now = Instant::now();
    let mut pending = request_map.lock();
    let before = pending.len();
    let stale_keys: Vec<String> = pending
        .iter()
        .filter_map(|(key, pending)| {
            if now.duration_since(pending.inserted_at).as_secs() > REQUEST_TTL_SECS {
                Some(key.clone())
            } else {
                None
            }
        })
        .collect();
    let mut removed_pending = Vec::with_capacity(stale_keys.len());
    for key in stale_keys {
        if let Some(request) = pending.remove(&key) {
            removed_pending.push(request);
        }
    }
    let after = pending.len();
    let removed = removed_pending.len();
    if removed > 0 {
        diagnostics::log(format!(
            "pool_stale_requests_cleaned removed={} remaining={}",
            removed, after
        ));
    }
    if before == after {
        return Vec::new();
    }
    removed_pending
}

async fn broadcast_to_all(line: &str, clients: &Arc<Mutex<HashMap<String, ClientSender>>>) {
    let senders: Vec<ClientSender> = clients.lock().values().cloned().collect();
    for sender in &senders {
        let _ = sender.send(line.to_string()).await;
    }
    diagnostics::log(format!(
        "pool_response_broadcast bytes={} clients={}",
        line.len(),
        senders.len()
    ));
}

/// True if the message carries a `method` field. JSON-RPC requests and
/// notifications have `method`; responses (carrying `result`/`error`) do not, so
/// this is the primary discriminator between the two families.
fn message_has_method(value: &Value) -> bool {
    value.get("method").is_some()
}

/// The message's `id` if present and non-null. A null/absent id marks a
/// notification; a non-null id marks a request (with `method`) or a response
/// (without `method`).
fn non_null_id(value: &Value) -> Option<&Value> {
    match value.get("id") {
        Some(id) if !id.is_null() => Some(id),
        _ => None,
    }
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

    // Build a last-active-client slot for route_response, optionally pre-set to a
    // specific client (the heuristic target for server-initiated requests).
    fn last_active(client_id: Option<&str>) -> Arc<Mutex<Option<String>>> {
        Arc::new(Mutex::new(client_id.map(str::to_string)))
    }

    // Fresh, empty per-upstream handshake cache for route_response tests.
    fn empty_cache() -> HandshakeCacheRef {
        Arc::new(Mutex::new(HandshakeCache::default()))
    }

    fn pending_request(
        client_id: &str,
        original_id: Value,
        method: Option<&str>,
    ) -> PendingRequestInfo {
        PendingRequestInfo {
            client_id: client_id.to_string(),
            original_id,
            method: method.map(str::to_string),
            inserted_at: Instant::now(),
        }
    }

    fn stale_pending_request(
        client_id: &str,
        original_id: Value,
        method: Option<&str>,
    ) -> PendingRequestInfo {
        PendingRequestInfo {
            client_id: client_id.to_string(),
            original_id,
            method: method.map(str::to_string),
            inserted_at: Instant::now() - Duration::from_secs(REQUEST_TTL_SECS + 1),
        }
    }

    async fn route_response(
        line: &str,
        clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
        request_map: &RequestMap,
        handshake_cache: &HandshakeCacheRef,
        cleanup_counter: &Arc<AtomicU32>,
        last_active_client: &Arc<Mutex<Option<String>>>,
    ) -> mpsc::Receiver<RecoveryReason> {
        let (recovery_tx, recovery_rx) = mpsc::channel::<RecoveryReason>(8);
        let recovery_requested = Arc::new(AtomicBool::new(false));
        let client_capabilities = Arc::new(Mutex::new(HashMap::new()));
        if let Some(client_id) = last_active_client.lock().clone() {
            client_capabilities.lock().insert(
                client_id,
                ClientCapabilities {
                    sampling: true,
                    roots: true,
                },
            );
        }
        let request_tx = Arc::new(Mutex::new(None));
        super::route_response(
            line,
            clients,
            request_map,
            handshake_cache,
            cleanup_counter,
            last_active_client,
            &client_capabilities,
            &request_tx,
            &recovery_tx,
            &recovery_requested,
        )
        .await;
        recovery_rx
    }

    async fn route_response_with_capabilities(
        line: &str,
        clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
        client_capabilities: &Arc<Mutex<HashMap<String, ClientCapabilities>>>,
        request_tx: &Arc<Mutex<Option<mpsc::Sender<String>>>>,
    ) {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        let cache = empty_cache();
        let counter = Arc::new(AtomicU32::new(0));
        let last_active = last_active(Some("clientA"));
        let (recovery_tx, _recovery_rx) = mpsc::channel::<RecoveryReason>(8);
        let recovery_requested = Arc::new(AtomicBool::new(false));
        super::route_response(
            line,
            clients,
            &request_map,
            &cache,
            &counter,
            &last_active,
            client_capabilities,
            request_tx,
            &recovery_tx,
            &recovery_requested,
        )
        .await;
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
            .insert("1".into(), pending_request("clientA", json!(1), None));
        request_map
            .lock()
            .insert("2".into(), pending_request("clientB", json!(1), None));

        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let mut rx_b = channel_client(&clients, "clientB");
        let counter = Arc::new(AtomicU32::new(0));
        let cache = empty_cache();
        let last_active = last_active(None);

        route_response(
            r#"{"jsonrpc":"2.0","id":1,"result":"A"}"#,
            &clients,
            &request_map,
            &cache,
            &counter,
            &last_active,
        )
        .await;
        route_response(
            r#"{"jsonrpc":"2.0","id":2,"result":"B"}"#,
            &clients,
            &request_map,
            &cache,
            &counter,
            &last_active,
        )
        .await;

        let a: Value =
            serde_json::from_str(&rx_a.try_recv().expect("clientA response")).expect("valid json");
        let b: Value =
            serde_json::from_str(&rx_b.try_recv().expect("clientB response")).expect("valid json");
        assert_eq!(a["id"], json!(1), "clientA original id restored");
        assert_eq!(a["result"], json!("A"));
        assert_eq!(b["id"], json!(1), "clientB original id restored");
        assert_eq!(b["result"], json!("B"));

        // Each client received exactly one message: no cross-wiring.
        assert!(rx_a.try_recv().is_err());
        assert!(rx_b.try_recv().is_err());
    }

    // A server-initiated request (method + an id the pool never issued) must
    // route to exactly ONE client — the heuristic last-active client — not
    // broadcast. Broadcasting would make every client answer the same request,
    // sending duplicate/conflicting responses upstream. The server's id is
    // preserved verbatim so the client's response id matches.
    #[tokio::test]
    async fn route_response_routes_server_request_to_single_client() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let mut rx_b = channel_client(&clients, "clientB");
        let counter = Arc::new(AtomicU32::new(0));
        // clientA is the most-recently-active client → the heuristic target.
        let last_active = last_active(Some("clientA"));

        route_response(
            r#"{"jsonrpc":"2.0","id":42,"method":"sampling/createMessage"}"#,
            &clients,
            &request_map,
            &empty_cache(),
            &counter,
            &last_active,
        )
        .await;

        let received: Value =
            serde_json::from_str(&rx_a.try_recv().expect("clientA receives server request"))
                .expect("valid json");
        assert_eq!(received["id"], json!(42), "server id preserved verbatim");
        assert_eq!(received["method"], json!("sampling/createMessage"));
        // Exactly one client answers: no broadcast.
        assert!(rx_b.try_recv().is_err(), "server request not broadcast");
    }

    #[tokio::test]
    async fn sampling_server_request_routes_to_sampling_capable_client() {
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let mut rx_b = channel_client(&clients, "clientB");
        let capabilities = Arc::new(Mutex::new(HashMap::new()));
        capabilities.lock().insert(
            "clientA".to_string(),
            ClientCapabilities {
                sampling: false,
                roots: false,
            },
        );
        capabilities.lock().insert(
            "clientB".to_string(),
            ClientCapabilities {
                sampling: true,
                roots: false,
            },
        );
        let request_tx = Arc::new(Mutex::new(None));

        route_response_with_capabilities(
            r#"{"jsonrpc":"2.0","id":42,"method":"sampling/createMessage"}"#,
            &clients,
            &capabilities,
            &request_tx,
        )
        .await;

        assert!(rx_a.try_recv().is_err(), "incapable last-active not used");
        let received: Value =
            serde_json::from_str(&rx_b.try_recv().expect("sampling client receives request"))
                .expect("valid json");
        assert_eq!(received["method"], json!("sampling/createMessage"));
    }

    #[tokio::test]
    async fn roots_server_request_routes_to_roots_capable_client() {
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let mut rx_b = channel_client(&clients, "clientB");
        let capabilities = Arc::new(Mutex::new(HashMap::new()));
        capabilities.lock().insert(
            "clientA".to_string(),
            ClientCapabilities {
                sampling: true,
                roots: false,
            },
        );
        capabilities.lock().insert(
            "clientB".to_string(),
            ClientCapabilities {
                sampling: false,
                roots: true,
            },
        );
        let request_tx = Arc::new(Mutex::new(None));

        route_response_with_capabilities(
            r#"{"jsonrpc":"2.0","id":43,"method":"roots/list"}"#,
            &clients,
            &capabilities,
            &request_tx,
        )
        .await;

        assert!(rx_a.try_recv().is_err(), "roots-incapable client not used");
        let received: Value =
            serde_json::from_str(&rx_b.try_recv().expect("roots client receives request"))
                .expect("valid json");
        assert_eq!(received["method"], json!("roots/list"));
    }

    #[tokio::test]
    async fn no_capable_client_sends_error_response_upstream() {
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let capabilities = Arc::new(Mutex::new(HashMap::new()));
        capabilities.lock().insert(
            "clientA".to_string(),
            ClientCapabilities {
                sampling: false,
                roots: false,
            },
        );
        let (upstream_tx, mut upstream_rx) = mpsc::channel::<String>(8);
        let request_tx = Arc::new(Mutex::new(Some(upstream_tx)));

        route_response_with_capabilities(
            r#"{"jsonrpc":"2.0","id":44,"method":"sampling/createMessage"}"#,
            &clients,
            &capabilities,
            &request_tx,
        )
        .await;

        assert!(
            rx_a.try_recv().is_err(),
            "request not sent to incapable client"
        );
        let response: Value =
            serde_json::from_str(&upstream_rx.try_recv().expect("upstream receives error"))
                .expect("valid json");
        assert_eq!(response["id"], json!(44));
        assert_eq!(response["error"]["code"], json!(-32001));
        assert_eq!(
            response["error"]["message"],
            json!("no capable downstream client connected for sampling/createMessage")
        );
        assert!(upstream_rx.try_recv().is_err(), "single upstream error");
    }

    // A pure notification (method, no id) fans out to every client.
    #[tokio::test]
    async fn route_response_broadcasts_notifications() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let mut rx_b = channel_client(&clients, "clientB");
        let counter = Arc::new(AtomicU32::new(0));
        let last_active = last_active(None);

        route_response(
            r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#,
            &clients,
            &request_map,
            &empty_cache(),
            &counter,
            &last_active,
        )
        .await;
        assert!(rx_a.try_recv().is_ok(), "notification fans out to clientA");
        assert!(rx_b.try_recv().is_ok(), "notification fans out to clientB");
    }

    // A response whose originating client has disconnected is dropped, never
    // rebroadcast (its restored id could collide with a live client's in-flight id).
    #[tokio::test]
    async fn route_response_drops_when_origin_client_gone() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        request_map
            .lock()
            .insert("5".into(), pending_request("ghost", json!(1), None));
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_other = channel_client(&clients, "other");
        let counter = Arc::new(AtomicU32::new(0));
        let last_active = last_active(None);

        route_response(
            r#"{"jsonrpc":"2.0","id":5,"result":"X"}"#,
            &clients,
            &request_map,
            &empty_cache(),
            &counter,
            &last_active,
        )
        .await;

        assert!(
            rx_other.try_recv().is_err(),
            "orphan response not broadcast"
        );
    }

    #[tokio::test]
    async fn empty_id_error_routes_to_oldest_pending_request() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        request_map.lock().insert(
            "10".into(),
            pending_request("clientA", json!(99), Some("tools/call")),
        );

        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let mut rx_b = channel_client(&clients, "clientB");
        let counter = Arc::new(AtomicU32::new(0));
        let last_active = last_active(None);

        route_response(
            r#"{"jsonrpc":"2.0","id":"","error":{"code":-32001,"message":"Session not found"}}"#,
            &clients,
            &request_map,
            &empty_cache(),
            &counter,
            &last_active,
        )
        .await;

        let routed: Value = serde_json::from_str(&rx_a.try_recv().expect("clientA receives error"))
            .expect("valid json");
        assert_eq!(routed["id"], json!(99), "original id restored");
        assert_eq!(routed["error"]["code"], json!(-32001));
        assert!(rx_b.try_recv().is_err(), "malformed error not broadcast");
        assert!(request_map.lock().is_empty(), "pending entry removed");
    }

    #[tokio::test]
    async fn empty_id_success_is_dropped_as_orphan() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        request_map.lock().insert(
            "10".into(),
            pending_request("clientA", json!(99), Some("tools/call")),
        );

        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let counter = Arc::new(AtomicU32::new(0));
        let last_active = last_active(None);

        route_response(
            r#"{"jsonrpc":"2.0","id":"","result":{"ok":true}}"#,
            &clients,
            &request_map,
            &empty_cache(),
            &counter,
            &last_active,
        )
        .await;

        assert!(
            rx_a.try_recv().is_err(),
            "empty-id success not fallback routed"
        );
        assert_eq!(request_map.lock().len(), 1, "pending request kept");
    }

    #[tokio::test]
    async fn empty_id_error_without_pending_request_is_orphaned() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let counter = Arc::new(AtomicU32::new(0));
        let last_active = last_active(None);

        route_response(
            r#"{"jsonrpc":"2.0","id":"","error":{"code":-32001,"message":"Session not found"}}"#,
            &clients,
            &request_map,
            &empty_cache(),
            &counter,
            &last_active,
        )
        .await;

        assert!(rx_a.try_recv().is_err(), "malformed error not broadcast");
    }

    #[tokio::test]
    async fn session_not_found_empty_id_error_clears_caches_and_signals_recovery() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        request_map.lock().insert(
            "10".into(),
            pending_request("clientA", json!(99), Some("tools/call")),
        );
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let _rx_a = channel_client(&clients, "clientA");
        let counter = Arc::new(AtomicU32::new(0));
        let last_active = last_active(None);
        let cache = empty_cache();
        cache
            .lock()
            .store("initialize", json!({"capabilities": {}}));
        cache.lock().store("tools/list", json!({"tools":["t1"]}));

        let mut recovery_rx = route_response(
            r#"{"jsonrpc":"2.0","id":"","error":{"code":-32001,"message":"Session not found"}}"#,
            &clients,
            &request_map,
            &cache,
            &counter,
            &last_active,
        )
        .await;

        assert_eq!(cache.lock().initialize, None);
        assert_eq!(cache.lock().tools_list.cached_result, None);
        assert_eq!(recovery_rx.try_recv(), Ok(RecoveryReason::SessionNotFound));
    }

    #[tokio::test]
    async fn non_session_error_does_not_signal_recovery() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        request_map.lock().insert(
            "10".into(),
            pending_request("clientA", json!(99), Some("tools/call")),
        );
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let _rx_a = channel_client(&clients, "clientA");
        let counter = Arc::new(AtomicU32::new(0));
        let last_active = last_active(None);
        let cache = empty_cache();
        cache
            .lock()
            .store("initialize", json!({"capabilities": {}}));

        let mut recovery_rx = route_response(
            r#"{"jsonrpc":"2.0","id":10,"error":{"code":-32001,"message":"rate limited"}}"#,
            &clients,
            &request_map,
            &cache,
            &counter,
            &last_active,
        )
        .await;

        assert_eq!(cache.lock().initialize, Some(json!({"capabilities": {}})));
        assert!(recovery_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn tool_call_session_not_found_is_returned_to_client() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        request_map.lock().insert(
            "10".into(),
            pending_request("clientA", json!(77), Some("tools/call")),
        );
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let counter = Arc::new(AtomicU32::new(0));
        let last_active = last_active(None);

        let _recovery_rx = route_response(
            r#"{"jsonrpc":"2.0","id":"","error":{"code":-32001,"message":"Session not found"}}"#,
            &clients,
            &request_map,
            &empty_cache(),
            &counter,
            &last_active,
        )
        .await;

        let routed: Value =
            serde_json::from_str(&rx_a.try_recv().expect("clientA receives concrete error"))
                .expect("valid json");
        assert_eq!(routed["id"], json!(77));
        assert_eq!(routed["error"]["message"], json!("Session not found"));
    }

    // The classifier helpers distinguish the three JSON-RPC message kinds by the
    // presence of `method` and a non-null `id`.
    #[test]
    fn classifier_helpers_distinguish_message_kinds() {
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/list"});
        assert!(message_has_method(&request));
        assert_eq!(non_null_id(&request), Some(&json!(1)));

        let notification = json!({"jsonrpc":"2.0","method":"notifications/progress"});
        assert!(message_has_method(&notification));
        assert_eq!(non_null_id(&notification), None);

        let response = json!({"jsonrpc":"2.0","id":7,"result":"ok"});
        assert!(!message_has_method(&response));
        assert_eq!(non_null_id(&response), Some(&json!(7)));

        // A null id is treated as absent (notification semantics).
        let null_id = json!({"jsonrpc":"2.0","id":null,"method":"x"});
        assert_eq!(non_null_id(&null_id), None);
    }

    // A cacheable handshake success response (result, no error) is cached when it
    // is routed back; an error response for the same method is not. Caching keys
    // off the method remembered in the pending request.
    #[tokio::test]
    async fn route_response_caches_success_not_error() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        // Pending tools/list (id 10) and initialize (id 11) successes, plus a
        // tools/list (id 12) that comes back as an error.
        request_map.lock().insert(
            "10".into(),
            pending_request("clientA", json!(1), Some("tools/list")),
        );
        request_map.lock().insert(
            "11".into(),
            pending_request("clientA", json!(2), Some("initialize")),
        );
        request_map.lock().insert(
            "12".into(),
            pending_request("clientA", json!(3), Some("tools/list")),
        );

        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let _rx_a = channel_client(&clients, "clientA");
        let counter = Arc::new(AtomicU32::new(0));
        let cache = empty_cache();
        let last_active = last_active(None);

        route_response(
            r#"{"jsonrpc":"2.0","id":10,"result":{"tools":["t1"]}}"#,
            &clients,
            &request_map,
            &cache,
            &counter,
            &last_active,
        )
        .await;
        route_response(
            r#"{"jsonrpc":"2.0","id":11,"result":{"capabilities":{}}}"#,
            &clients,
            &request_map,
            &cache,
            &counter,
            &last_active,
        )
        .await;
        // Error response must NOT overwrite/populate the cache.
        route_response(
            r#"{"jsonrpc":"2.0","id":12,"error":{"code":-32001,"message":"rate limited"}}"#,
            &clients,
            &request_map,
            &cache,
            &counter,
            &last_active,
        )
        .await;

        let guard = cache.lock();
        assert_eq!(
            guard.get("tools/list"),
            Some(json!({"tools":["t1"]})),
            "tools/list success cached"
        );
        assert_eq!(
            guard.get("initialize"),
            Some(json!({"capabilities":{}})),
            "initialize success cached"
        );
        // The error did not replace the earlier cached tools/list success.
        assert_eq!(
            guard.get("tools/list"),
            Some(json!({"tools":["t1"]})),
            "error response not cached"
        );
    }

    // An error response for a method with an EMPTY cache leaves the cache empty —
    // errors (429, auth, -32001) are never cached.
    #[tokio::test]
    async fn route_response_does_not_cache_error_into_empty_cache() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        request_map.lock().insert(
            "20".into(),
            pending_request("clientA", json!(1), Some("tools/list")),
        );
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let _rx_a = channel_client(&clients, "clientA");
        let counter = Arc::new(AtomicU32::new(0));
        let cache = empty_cache();
        let last_active = last_active(None);

        route_response(
            r#"{"jsonrpc":"2.0","id":20,"error":{"code":429,"message":"too many"}}"#,
            &clients,
            &request_map,
            &cache,
            &counter,
            &last_active,
        )
        .await;

        assert_eq!(cache.lock().get("tools/list"), None, "error not cached");
    }

    // An upstream notifications/tools/list_changed invalidates ONLY the tools/list
    // cache; initialize stays cached. The notification is still broadcast.
    #[tokio::test]
    async fn tools_list_cache_invalidated_on_list_changed() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        let clients: Arc<Mutex<HashMap<String, ClientSender>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let mut rx_a = channel_client(&clients, "clientA");
        let counter = Arc::new(AtomicU32::new(0));
        let last_active = last_active(None);

        let cache = empty_cache();
        cache.lock().store("tools/list", json!({"tools":["t1"]}));
        cache.lock().store("initialize", json!({"capabilities":{}}));

        route_response(
            r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#,
            &clients,
            &request_map,
            &cache,
            &counter,
            &last_active,
        )
        .await;

        assert_eq!(
            cache.lock().get("tools/list"),
            None,
            "tools/list invalidated"
        );
        assert_eq!(
            cache.lock().get("initialize"),
            Some(json!({"capabilities":{}})),
            "initialize not invalidated"
        );
        // The notification is still broadcast to clients.
        assert!(rx_a.try_recv().is_ok(), "list_changed still broadcast");
    }

    // The cache-hit path builds a fresh JSON-RPC response stamped with the new
    // client's original id (not the id used when the entry was first discovered),
    // so a second client is served from cache without going upstream.
    #[test]
    fn cached_response_restored_with_new_client_id() {
        let cache = Arc::new(Mutex::new(HandshakeCache::default()));
        cache
            .lock()
            .store("tools/list", json!({"tools":["t1","t2"]}));

        // Simulate a new client whose original request id is 99.
        let result = cache.lock().get("tools/list").expect("tools/list cached");
        let line = build_success_response(json!(99), result);
        let parsed: Value = serde_json::from_str(&line).expect("valid json");

        assert_eq!(parsed["jsonrpc"], json!("2.0"));
        assert_eq!(parsed["id"], json!(99), "new client's id stamped");
        assert_eq!(parsed["result"], json!({"tools":["t1","t2"]}));
        assert!(parsed.get("error").is_none(), "cached reply is a success");
    }

    // maybe_cache_response only caches success responses for cacheable methods.
    #[test]
    fn maybe_cache_response_filters_by_method_and_success() {
        let cache = empty_cache();

        // Non-cacheable method: ignored even on success.
        maybe_cache_response(
            &json!({"jsonrpc":"2.0","id":1,"result":"x"}),
            "resources/list",
            &cache,
        );
        assert_eq!(cache.lock().get("tools/list"), None);

        // Cacheable method, success: cached.
        maybe_cache_response(
            &json!({"jsonrpc":"2.0","id":1,"result":{"tools":[]}}),
            "tools/list",
            &cache,
        );
        assert_eq!(cache.lock().get("tools/list"), Some(json!({"tools":[]})));

        // Cacheable method, error: not cached (and does not overwrite).
        maybe_cache_response(
            &json!({"jsonrpc":"2.0","id":2,"error":{"code":-32001}}),
            "tools/list",
            &cache,
        );
        assert_eq!(
            cache.lock().get("tools/list"),
            Some(json!({"tools":[]})),
            "error left prior success untouched"
        );
    }

    #[test]
    fn concurrent_tools_list_misses_create_one_leader_and_one_waiter() {
        let cache = empty_cache();

        let first = prepare_tools_list_request(&cache, "clientA", json!(1));
        let second = prepare_tools_list_request(&cache, "clientB", json!(2));

        assert!(matches!(first, ToolsListAction::Leader));
        assert!(matches!(second, ToolsListAction::Coalesced));
        let guard = cache.lock();
        assert!(guard.tools_list.in_flight, "leader owns upstream discovery");
        assert_eq!(guard.tools_list.waiters.len(), 1, "follower waits");
        assert_eq!(guard.tools_list.waiters[0].client_id, "clientB");
    }

    #[test]
    fn successful_tools_list_leader_fans_out_and_populates_cache() {
        let cache = empty_cache();
        cache.lock().tools_list.in_flight = true;
        cache.lock().tools_list.waiters.push(PendingWaiter {
            client_id: "clientB".to_string(),
            original_id: json!(2),
            inserted_at: Instant::now(),
        });
        let leader = pending_request("clientA", json!(1), Some("tools/list"));

        let responses = complete_tools_list_response(
            &json!({"jsonrpc":"2.0","id":10,"result":{"tools":["t1"]}}),
            leader,
            &cache,
        );

        assert_eq!(responses.len(), 2, "leader and waiter both answered");
        let leader_response: Value = serde_json::from_str(&responses[0].1).expect("valid json");
        let waiter_response: Value = serde_json::from_str(&responses[1].1).expect("valid json");
        assert_eq!(leader_response["id"], json!(1));
        assert_eq!(waiter_response["id"], json!(2));
        let guard = cache.lock();
        assert!(!guard.tools_list.in_flight);
        assert!(guard.tools_list.waiters.is_empty());
        assert_eq!(
            guard.tools_list.cached_result,
            Some(json!({"tools":["t1"]}))
        );
        assert_eq!(
            guard.tools_list.last_good_result,
            Some(json!({"tools":["t1"]}))
        );
    }

    #[test]
    fn stale_tools_list_leader_clears_in_flight_and_drains_waiters() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        request_map.lock().insert(
            "10".to_string(),
            stale_pending_request("leader", json!(1), Some("tools/list")),
        );
        let cleanup_counter = Arc::new(AtomicU32::new(0));
        let cache = empty_cache();
        cache.lock().tools_list.in_flight = true;
        cache.lock().tools_list.waiters.push(PendingWaiter {
            client_id: "waiter".to_string(),
            original_id: json!(2),
            inserted_at: Instant::now(),
        });

        let stale_requests = cleanup_stale_requests(&request_map, &cleanup_counter);
        let responses = cleanup_tools_list_after_stale_requests(&cache, &stale_requests);

        assert!(request_map.lock().is_empty(), "stale leader removed");
        assert_eq!(responses.len(), 1, "waiter receives timeout error");
        let timeout: Value = serde_json::from_str(&responses[0].1).expect("valid json");
        assert_eq!(responses[0].0, "waiter");
        assert_eq!(timeout["id"], json!(2));
        assert_eq!(timeout["error"]["code"], json!(-32001));
        assert_eq!(
            timeout["error"]["message"],
            json!("tools/list discovery timed out")
        );
        assert!(!cache.lock().tools_list.in_flight, "in-flight cleared");
        assert!(
            cache.lock().tools_list.waiters.is_empty(),
            "waiters drained"
        );

        let next = prepare_tools_list_request(&cache, "next", json!(3));

        assert!(
            matches!(next, ToolsListAction::Leader),
            "future miss can elect a new leader"
        );
        assert!(
            cache.lock().tools_list.in_flight,
            "new leader owns discovery"
        );
    }

    #[test]
    fn failed_tools_list_leader_fans_out_without_caching_error() {
        let cache = empty_cache();
        cache.lock().tools_list.in_flight = true;
        cache.lock().tools_list.last_good_result = Some(json!({"tools":["old"]}));
        cache.lock().tools_list.waiters.push(PendingWaiter {
            client_id: "clientB".to_string(),
            original_id: json!(2),
            inserted_at: Instant::now(),
        });
        let leader = pending_request("clientA", json!(1), Some("tools/list"));

        let responses = complete_tools_list_response(
            &json!({"jsonrpc":"2.0","id":10,"error":{"code":429,"message":"too many"}}),
            leader,
            &cache,
        );

        assert_eq!(responses.len(), 2);
        let leader_response: Value = serde_json::from_str(&responses[0].1).expect("valid json");
        let waiter_response: Value = serde_json::from_str(&responses[1].1).expect("valid json");
        assert_eq!(leader_response["id"], json!(1));
        assert_eq!(waiter_response["id"], json!(2));
        assert_eq!(leader_response["error"]["code"], json!(429));
        let guard = cache.lock();
        assert_eq!(guard.tools_list.cached_result, None, "error not cached");
        assert_eq!(
            guard.tools_list.last_good_result,
            Some(json!({"tools":["old"]})),
            "last-good snapshot kept"
        );
    }

    #[test]
    fn later_success_repopulates_tools_list_cache_after_failure() {
        let cache = empty_cache();
        cache.lock().tools_list.in_flight = true;
        let failed_leader = pending_request("clientA", json!(1), Some("tools/list"));
        let _responses = complete_tools_list_response(
            &json!({"jsonrpc":"2.0","id":10,"error":{"code":429,"message":"too many"}}),
            failed_leader,
            &cache,
        );
        cache.lock().tools_list.in_flight = true;

        let successful_leader = pending_request("clientA", json!(3), Some("tools/list"));
        let _responses = complete_tools_list_response(
            &json!({"jsonrpc":"2.0","id":11,"result":{"tools":["new"]}}),
            successful_leader,
            &cache,
        );

        assert_eq!(
            cache.lock().tools_list.cached_result,
            Some(json!({"tools":["new"]}))
        );
    }
}
