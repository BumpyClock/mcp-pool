use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, mpsc, watch};
use tokio::time::{Duration, sleep};

use crate::diagnostics;
use crate::jsonrpc::{self, IdAllocator};
#[cfg(test)]
use crate::mcp_session::cacheable_method;
use crate::mcp_session::{
    CacheableMethod, ClientCapabilities, HandshakeCache, Initialization, PendingRequestInfo,
    PendingWaiter, RecoveryReason, build_error_response, build_success_response, cacheable_request,
    is_session_not_found_error, parse_client_capabilities, tool_name,
};
use crate::transport::{LocalListener, LocalStream};
use crate::types::{ServerReadiness, ServerStatus};
use crate::upstream::{UpstreamHandle, UpstreamSpec};

#[path = "socket_proxy_cache.rs"]
mod cache;
#[path = "socket_proxy_client.rs"]
mod client;
#[path = "socket_proxy_dispatch.rs"]
mod dispatch;
#[path = "socket_proxy_generation.rs"]
mod generation;
#[path = "socket_proxy_router.rs"]
mod router;
#[path = "socket_proxy_wait.rs"]
mod wait;

use cache::*;
use client::handle_client;
use dispatch::*;
use generation::Generation;
use router::route_response;
use wait::acquire_request_sender;

const REQUEST_TTL_SECS: u64 = 300;
type ClientSender = mpsc::Sender<String>;
type RequestMap = Arc<Mutex<HashMap<String, PendingRequestInfo>>>;
type HandshakeCacheRef = Arc<Mutex<HandshakeCache>>;
type Completion = Option<Result<(), String>>;

enum ClientAction {
    Forward {
        line: String,
        method: Option<String>,
        tool: Option<String>,
        pool_id: Option<u64>,
    },
    Cached(String),
    Drop,
}

enum DiscoveryAction {
    Cached(String),
    Coalesced,
    Leader,
}

/// The lifecycle lock serializes mutations; each run owns isolated routing state.
/// Completion means both backend retirement and local task retirement are verified.
pub struct SocketProxy {
    name: String,
    socket_path: PathBuf,
    spec: UpstreamSpec,
    owned: bool,
    operation: tokio::sync::Mutex<()>,
    stop_requested: AtomicBool,
    status: Arc<Mutex<ServerStatus>>,
    generation: Mutex<Option<Arc<Generation>>>,
    started_at: Arc<Mutex<Option<Instant>>>,
    #[cfg(test)]
    test_setup: Mutex<Option<generation::TestSetup>>,
}

impl SocketProxy {
    pub fn new(name: String, socket_path: PathBuf, spec: UpstreamSpec, owned: bool) -> Self {
        Self {
            name,
            socket_path,
            spec,
            owned,
            operation: tokio::sync::Mutex::new(()),
            stop_requested: AtomicBool::new(false),
            status: Arc::new(Mutex::new(if owned {
                ServerStatus::Stopped
            } else {
                ServerStatus::Running
            })),
            generation: Mutex::new(None),
            started_at: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            test_setup: Mutex::new(None),
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
        self.generation
            .lock()
            .as_ref()
            .map_or(0, |generation| generation.clients.lock().len() as u32)
    }

    pub fn readiness(&self) -> ServerReadiness {
        let generation = self.generation.lock().clone();
        let Some(generation) = generation else {
            // A discovered socket proves only the local endpoint exists.
            return ServerReadiness {
                local_socket_bound: !self.owned && self.status() == ServerStatus::Running,
                ..ServerReadiness::default()
            };
        };
        let startup = generation.startup.borrow().clone();
        let completion = generation.completion.borrow().clone();
        ServerReadiness {
            local_socket_bound: generation.socket_bound.load(Ordering::SeqCst),
            upstream_transport_ready: matches!(startup, Some(Ok(())))
                && completion.is_none()
                && self.status() == ServerStatus::Running
                && !generation.shutdown.load(Ordering::SeqCst),
            mcp_initialize_result_received: matches!(
                &generation.handshake_cache.lock().initialize,
                Initialization::Ready { .. },
            ),
            startup_error: startup.and_then(Result::err),
            retirement_error: completion.and_then(Result::err),
        }
    }

    pub async fn start(self: &Arc<Self>) -> io::Result<()> {
        let _operation = self.operation.lock().await;
        self.start_locked().await
    }

    async fn start_locked(self: &Arc<Self>) -> io::Result<()> {
        let previous = self.generation.lock().clone();
        if let Some(previous) = previous.as_ref()
            && let Some(Err(error)) = previous.completion.borrow().clone()
        {
            return Err(io::Error::other(error));
        }
        if self.stop_requested.load(Ordering::SeqCst) {
            return Err(io::Error::other("pool stopped during upstream startup"));
        }
        if !self.owned {
            *self.status.lock() = ServerStatus::Running;
            return Ok(());
        }
        if let Some(previous) = previous {
            if !previous.shutdown.load(Ordering::SeqCst) && previous.completion.borrow().is_none() {
                return generation::wait_completion(previous.startup.clone()).await;
            }
            // A stopped status alone cannot prove that an old process is gone.
            generation::wait_completion(previous.completion.clone()).await?;
        }

        *self.status.lock() = ServerStatus::Starting;
        let listener = match crate::transport::bind(&self.socket_path) {
            Ok(listener) => Arc::new(listener),
            Err(error) => {
                *self.status.lock() = ServerStatus::Stopped;
                return Err(error);
            }
        };
        let generation = Arc::new(Generation::new());
        *self.generation.lock() = Some(generation.clone());
        if self.stop_requested.load(Ordering::SeqCst) {
            generation.signal_shutdown();
        }
        diagnostics::log(format!(
            "pool_proxy_starting name={} transport={}",
            self.name,
            self.transport()
        ));
        generation::spawn_owner(self, generation.clone(), listener);
        generation::wait_completion(generation.startup.clone()).await
    }

    pub async fn stop(&self) -> io::Result<()> {
        // Signal before acquiring the lock so a stop can retire an in-progress spawn.
        self.request_stop();
        let _operation = self.operation.lock().await;
        self.stop_locked().await
    }

    pub fn request_stop(&self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        let generation = self.generation.lock().clone();
        if let Some(generation) = generation {
            generation.signal_shutdown();
        }
    }

    async fn stop_locked(&self) -> io::Result<()> {
        let generation = self.generation.lock().clone();
        if let Some(generation) = generation {
            generation.signal_shutdown();
            generation::wait_completion(generation.completion.clone()).await?;
        }
        *self.status.lock() = ServerStatus::Stopped;
        self.stop_requested.store(false, Ordering::SeqCst);
        Ok(())
    }

    pub async fn restart(self: &Arc<Self>) -> io::Result<bool> {
        if !self.owned {
            return Ok(false);
        }
        let _operation = self.operation.lock().await;
        self.stop_locked().await?;
        self.start_locked().await?;
        Ok(true)
    }

    async fn recover(self: &Arc<Self>, previous: Arc<Generation>) {
        let _operation = self.operation.lock().await;
        let current = self.generation.lock().clone();
        if previous.explicit_stop.load(Ordering::SeqCst)
            || !current.is_some_and(|current| Arc::ptr_eq(&current, &previous))
        {
            return;
        }
        if let Err(error) = generation::wait_completion(previous.completion.clone()).await {
            diagnostics::log(format!(
                "pool_recovery_failed name={} error={error}",
                self.name
            ));
            return;
        }
        if let Err(error) = self.start_locked().await {
            diagnostics::log(format!(
                "pool_recovery_failed name={} error={error}",
                self.name
            ));
        }
    }
}

impl Drop for SocketProxy {
    fn drop(&mut self) {
        self.request_stop();
    }
}

#[cfg(test)]
#[path = "socket_proxy_cursor_tests.rs"]
mod cursor_tests;
#[cfg(test)]
#[path = "socket_proxy_initialize_http_tests.rs"]
mod initialize_http_tests;
#[cfg(test)]
#[path = "socket_proxy_initialize_tests.rs"]
mod initialize_tests;
#[cfg(test)]
#[path = "socket_proxy_lifecycle_tests.rs"]
mod lifecycle_tests;
#[cfg(test)]
#[path = "socket_proxy_retirement_tests.rs"]
pub(crate) mod retirement_tests;
#[cfg(test)]
#[path = "socket_proxy_terminal_tests.rs"]
mod terminal_tests;
#[cfg(test)]
#[path = "socket_proxy_tests_1.rs"]
mod tests_1;
#[cfg(test)]
#[path = "socket_proxy_tests_2.rs"]
mod tests_2;
#[cfg(test)]
#[path = "socket_proxy_tests_3.rs"]
mod tests_3;
