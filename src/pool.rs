use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::RwLock;
use std::collections::HashMap;

use crate::config::ServerDef;
use crate::socket_proxy::SocketProxy;
use crate::types::PoolStatusResponse;
use crate::upstream::UpstreamSpec;

/// Registry of pooled MCP servers. Each entry owns one `SocketProxy` (one
/// upstream + one bound socket). The daemon holds a single `Pool`.
pub struct Pool {
    proxies: RwLock<HashMap<String, Arc<SocketProxy>>>,
}

impl Pool {
    pub fn new() -> Self {
        Self {
            proxies: RwLock::new(HashMap::new()),
        }
    }

    pub fn is_running(&self, name: &str) -> bool {
        let _ = name;
        false
    }

    pub fn socket_path(&self, name: &str) -> Option<PathBuf> {
        let _ = name;
        None
    }

    pub fn start(&self, name: &str, spec: UpstreamSpec) -> std::io::Result<()> {
        let _ = (name, spec);
        todo!("build SocketProxy, bind socket, start; skip if already present")
    }

    pub fn stop_server(&self, name: &str) -> std::io::Result<bool> {
        let _ = name;
        todo!()
    }

    pub async fn restart(&self, name: &str) -> std::io::Result<bool> {
        let _ = name;
        todo!()
    }

    pub fn shutdown(&self) {
        todo!("stop all proxies, clear registry")
    }

    pub async fn wait_for_socket(&self, name: &str, timeout: Duration) -> bool {
        let _ = (name, timeout);
        false
    }

    pub fn discover_existing_sockets(&self) -> usize {
        0
    }

    pub fn get_status(&self) -> PoolStatusResponse {
        PoolStatusResponse::default()
    }
}

/// Build the upstream specification from a configured server definition.
pub fn upstream_spec_from_def(def: &ServerDef) -> UpstreamSpec {
    if def.is_remote() {
        UpstreamSpec::Http {
            url: def.url.clone(),
            sse: def.transport.eq_ignore_ascii_case("sse"),
        }
    } else {
        UpstreamSpec::Stdio {
            command: def.command.clone(),
            args: def.args.clone(),
            env: def.env.clone(),
        }
    }
}

/// Probe whether a socket endpoint has a live listener.
pub fn socket_alive(path: &Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(path).is_ok()
    }
    #[cfg(windows)]
    {
        tokio::net::windows::named_pipe::ClientOptions::new()
            .open(path.to_string_lossy().as_ref())
            .is_ok()
    }
}
