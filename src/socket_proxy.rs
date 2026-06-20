use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use parking_lot::Mutex;
use tokio::sync::{Notify, oneshot};

use crate::types::ServerStatus;
use crate::upstream::UpstreamSpec;

/// One pooled MCP: a single upstream multiplexed across many agent clients.
/// Each client connects over the bound local socket; requests are forwarded to
/// the upstream and responses routed back by JSON-RPC `id` (id-less messages
/// broadcast to all clients). See `crate::upstream` for the backend.
pub struct SocketProxy {
    _private: (),
}

impl SocketProxy {
    pub fn new(name: String, socket_path: PathBuf, spec: UpstreamSpec, owned: bool) -> Self {
        let _ = (name, socket_path, spec, owned);
        Self { _private: () }
    }

    pub fn start(&self) -> std::io::Result<()> {
        todo!("spawn upstream + response router + accept loop; set Running + ready_notify")
    }

    pub fn stop(&self) -> std::io::Result<()> {
        todo!("signal shutdown, drop listener, kill upstream")
    }

    pub async fn restart(&self) -> std::io::Result<bool> {
        todo!("stop, await exit, start")
    }

    pub fn status(&self) -> ServerStatus {
        ServerStatus::Stopped
    }

    pub fn socket_path(&self) -> PathBuf {
        PathBuf::new()
    }

    pub fn is_owned(&self) -> bool {
        false
    }

    pub fn uptime_seconds(&self) -> Option<u64> {
        None
    }

    pub fn connection_count(&self) -> u32 {
        0
    }

    pub fn ready_notifier(&self) -> Arc<Notify> {
        todo!()
    }

    pub fn take_exit_receiver(&self) -> Option<oneshot::Receiver<()>> {
        None
    }
}

/// Type alias kept for callers that track the multiplexer start time.
pub type ProxyStartedAt = Instant;

/// Marker so unused imports during the stub phase do not become hard errors.
#[allow(dead_code)]
fn _retain(_notify: Arc<Notify>, _mutex: Mutex<()>) {}
