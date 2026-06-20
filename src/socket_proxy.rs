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
/// original JSON-RPC id (restored on the matching response), the method name
/// when it is a cacheable handshake method (so `route_response` knows whether to
/// cache the success response), and when it was registered (for TTL cleanup).
/// Keyed in the request map by the pool-unique id.
type PendingRequest = (String, Value, Option<String>, Instant);
type RequestMap = Arc<Mutex<HashMap<String, PendingRequest>>>;

/// Per-upstream cache of successful handshake/discovery responses. mcpproxy-go
/// owns upstream discovery in the proxy; here we serve repeat `initialize` and
/// `tools/list` requests from proxy-side state so each new downstream client does
/// not re-trigger an upstream round-trip (which, for auth'd upstreams, would
/// re-acquire tokens and risk 429 retries). The cached value is the response
/// `result` payload; it is re-id'd per client when served. The cache is per
/// SocketProxy/upstream, never global across servers.
#[derive(Default)]
struct HandshakeCache {
    initialize: Option<Value>,
    tools_list: Option<Value>,
}

impl HandshakeCache {
    /// Cached `result` for a method, if present. Only the two handshake methods
    /// are ever cached.
    fn get(&self, method: &str) -> Option<Value> {
        match method {
            "initialize" => self.initialize.clone(),
            "tools/list" => self.tools_list.clone(),
            _ => None,
        }
    }

    /// Store a successful `result` for a cacheable method. No-op for others.
    fn store(&mut self, method: &str, result: Value) {
        match method {
            "initialize" => self.initialize = Some(result),
            "tools/list" => self.tools_list = Some(result),
            _ => {}
        }
    }

    /// Drop only the tools/list entry (initialize stays valid) on an upstream
    /// `notifications/tools/list_changed`.
    fn invalidate_tools_list(&mut self) {
        self.tools_list = None;
    }
}

type HandshakeCacheRef = Arc<Mutex<HandshakeCache>>;

/// True for the handshake/discovery methods whose success responses are cached.
fn is_cacheable_method(method: &str) -> bool {
    method == "initialize" || method == "tools/list"
}

/// Build a JSON-RPC success response line for a cached `result`, stamped with the
/// requesting client's original id. Panic-free string construction.
fn build_cached_response(original_id: Value, result: Value) -> String {
    let mut object = serde_json::Map::new();
    object.insert("jsonrpc".to_string(), Value::from("2.0"));
    object.insert("id".to_string(), original_id);
    object.insert("result".to_string(), result);
    Value::Object(object).to_string()
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

/// What to do with a parsed client line: forward it upstream, or reply directly
/// from cache without an upstream round-trip.
enum ClientAction {
    Forward(String),
    Cached(String),
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
        Self {
            name, socket_path, spec, owned,
            status: Arc::new(Mutex::new(ServerStatus::Stopped)),
            request_tx: Arc::new(Mutex::new(None)),
            listener: Mutex::new(None),
            clients: Arc::new(Mutex::new(HashMap::new())),
            request_map: Arc::new(Mutex::new(HashMap::new())),
            handshake_cache: Arc::new(Mutex::new(HandshakeCache::default())),
            last_active_client: Arc::new(Mutex::new(None)),
            id_allocator: Arc::new(IdAllocator::new()),
            shutdown: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(Notify::new()),
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

    pub fn start(&self) -> io::Result<()> {
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

        diagnostics::log(format!("pool_proxy_starting name={} transport={}", self.name, self.transport()));

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

        // Optimistically Running: the upstream task flips us to Stopped on spawn
        // failure or exit, overriding this. Bind success + tasks spawned == ready.
        *self.status.lock() = ServerStatus::Running;
        *self.started_at.lock() = Some(Instant::now());

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
        let upstream_ready = self.upstream_ready.clone();
        let clients = self.clients.clone();
        let request_map = self.request_map.clone();
        let handshake_cache = self.handshake_cache.clone();
        let last_active_client = self.last_active_client.clone();
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
                route_response(&message, &clients, &request_map, &handshake_cache, &cleanup_counter, &last_active_client).await;
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
                        diagnostics::log(format!("pool_client_connected name={} client_id={}", name, client_id));

                        let clients_for_drop = clients.clone();
                        let request_map_for_drop = request_map.clone();
                        let handshake_cache_for_client = handshake_cache.clone();
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
    handshake_cache: HandshakeCacheRef,
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
                                    // Serve cacheable handshake methods from the
                                    // proxy-side cache so a new client does not
                                    // re-trigger upstream discovery/auth.
                                    let cached = method.as_deref().and_then(|m| {
                                        if is_cacheable_method(m) {
                                            handshake_cache.lock().get(m).map(|r| (m.to_string(), r))
                                        } else {
                                            None
                                        }
                                    });
                                    if let Some((method, result)) = cached {
                                        diagnostics::log(format!(
                                            "pool_cache_hit method={} client_id={}",
                                            method, client_id
                                        ));
                                        ClientAction::Cached(build_cached_response(
                                            original_id,
                                            result,
                                        ))
                                    } else {
                                        let pool_id = id_allocator.allocate();
                                        // Remember the method only for cacheable
                                        // ones, so route_response can cache the
                                        // matching success response.
                                        let cacheable = method
                                            .filter(|m| is_cacheable_method(m));
                                        // Key the pending request through the same
                                        // canonical helper route_response uses to
                                        // look it up, so the insert and lookup keys
                                        // cannot drift (numeric id -> identical
                                        // string).
                                        request_map.lock().insert(
                                            jsonrpc::id_key(&Value::from(pool_id)),
                                            (
                                                client_id.clone(),
                                                original_id,
                                                cacheable,
                                                Instant::now(),
                                            ),
                                        );
                                        // Record this client as most-recently-active
                                        // so a server-initiated callback can route
                                        // back to it (see route_server_request).
                                        *last_active_client.lock() = Some(client_id.clone());
                                        match value {
                                            Value::Object(object) => ClientAction::Forward(
                                                jsonrpc::with_id(object, Value::from(pool_id)),
                                            ),
                                            // Unreachable: guarded by is_object
                                            // above, but match instead of unwrap to
                                            // stay panic-free.
                                            _ => ClientAction::Forward(line.clone()),
                                        }
                                    }
                                }
                                // NOTIFICATION (method, no id) or RESPONSE
                                // (no method): forward verbatim, never store.
                                _ => ClientAction::Forward(line.clone()),
                            }
                        }
                        Ok(_) => ClientAction::Forward(line.clone()),
                        Err(_) => {
                            if parse_failures < 3 {
                                // Throttle log spam from a chatty malformed sender.
                                parse_failures += 1;
                                diagnostics::log(format!(
                                    "pool_request_parse_failed client_id={} bytes={}",
                                    client_id, line.len()
                                ));
                            }
                            ClientAction::Forward(line.clone())
                        }
                    };

                    let forward_line = match action {
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
                        ClientAction::Forward(forward_line) => forward_line,
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
    request_map.lock().retain(|_, (cid, _, _, _)| cid != &client_id);
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
async fn route_response(
    line: &str,
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    request_map: &RequestMap,
    handshake_cache: &HandshakeCacheRef,
    cleanup_counter: &Arc<AtomicU32>,
    last_active_client: &Arc<Mutex<Option<String>>>,
) {
    cleanup_stale_requests(request_map, cleanup_counter);

    match serde_json::from_str::<Value>(line) {
        Ok(value) if value.is_object() => {
            // Clone the id (ending the borrow) before potentially moving the
            // object into `with_id`.
            let id = non_null_id(&value).cloned();
            match (message_has_method(&value), id) {
                // SERVER-INITIATED REQUEST: method + id. Route to one client.
                (true, Some(_)) => {
                    route_server_request(line, clients, last_active_client).await;
                }
                // RESPONSE: no method, has id. Restore the client's original id.
                (false, Some(id)) => {
                    let key = jsonrpc::id_key(&id);
                    let restored = match request_map.lock().remove(&key) {
                        Some((client_id, original_id, method, _)) => {
                            // Cache successful handshake/discovery responses
                            // (result, no error) so later clients are served from
                            // proxy-side state. Errors are never cached.
                            if let Some(method) = method.as_deref() {
                                maybe_cache_response(&value, method, handshake_cache);
                            }
                            match value {
                                Value::Object(object) => {
                                    Some((client_id, jsonrpc::with_id(object, original_id)))
                                }
                                // Unreachable: guarded by is_object above.
                                _ => None,
                            }
                        }
                        None => None,
                    };
                    match restored {
                        Some((client_id, payload)) => {
                            send_to_client(&client_id, payload, clients).await;
                        }
                        None => {
                            // Orphan/late response: the request is gone (TTL
                            // cleanup or client disconnect). Drop, never broadcast.
                            diagnostics::log(format!(
                                "pool_response_orphaned id={} reason=no_pending_request",
                                key
                            ));
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
    clients: &Arc<Mutex<HashMap<String, ClientSender>>>,
    last_active_client: &Arc<Mutex<Option<String>>>,
) {
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
    diagnostics::log(format!("pool_server_request_routed client_id={}", client_id));
    send_to_client(&client_id, line.to_string(), clients).await;
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
fn cleanup_stale_requests(request_map: &RequestMap, cleanup_counter: &Arc<AtomicU32>) {
    let count = cleanup_counter.fetch_add(1, Ordering::Relaxed);
    if !count.is_multiple_of(CLEANUP_INTERVAL) {
        return;
    }

    let now = Instant::now();
    let before = request_map.lock().len();
    request_map.lock().retain(|_, (_, _, _, inserted_at)| {
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

    // The core multiplexing fix: two clients that independently used the same
    // raw id (1) are tracked under distinct pool ids, so their responses route
    // back to the right client with each client's original id restored — no
    // cross-wiring, no broadcast.
    #[tokio::test]
    async fn route_response_restores_ids_without_cross_wiring() {
        let request_map: RequestMap = Arc::new(Mutex::new(HashMap::new()));
        request_map
            .lock()
            .insert("1".into(), ("clientA".into(), json!(1), None, Instant::now()));
        request_map
            .lock()
            .insert("2".into(), ("clientB".into(), json!(1), None, Instant::now()));

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
            .insert("5".into(), ("ghost".into(), json!(1), None, Instant::now()));
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

        assert!(rx_other.try_recv().is_err(), "orphan response not broadcast");
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
            ("clientA".into(), json!(1), Some("tools/list".into()), Instant::now()),
        );
        request_map.lock().insert(
            "11".into(),
            ("clientA".into(), json!(2), Some("initialize".into()), Instant::now()),
        );
        request_map.lock().insert(
            "12".into(),
            ("clientA".into(), json!(3), Some("tools/list".into()), Instant::now()),
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
            ("clientA".into(), json!(1), Some("tools/list".into()), Instant::now()),
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

        assert_eq!(cache.lock().get("tools/list"), None, "tools/list invalidated");
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
        cache.lock().store("tools/list", json!({"tools":["t1","t2"]}));

        // Simulate a new client whose original request id is 99.
        let result = cache
            .lock()
            .get("tools/list")
            .expect("tools/list cached");
        let line = build_cached_response(json!(99), result);
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
}
