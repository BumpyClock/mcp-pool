use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;

use crate::config::ServerDef;
use crate::socket_proxy::SocketProxy;
use crate::types::PoolStatusResponse;
use crate::upstream::UpstreamSpec;

/// Registry of pooled MCP servers. Each entry owns one `SocketProxy` (one
/// upstream + one bound socket). The daemon holds a single `Pool`.
pub struct Pool {
    proxies: RwLock<HashMap<String, Arc<SocketProxy>>>,
    operations: RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    shutdown_gate: tokio::sync::RwLock<()>,
    shutting_down: AtomicBool,
}

impl Pool {
    pub fn new() -> Self {
        Self {
            proxies: RwLock::new(HashMap::new()),
            operations: RwLock::new(HashMap::new()),
            shutdown_gate: tokio::sync::RwLock::new(()),
            shutting_down: AtomicBool::new(false),
        }
    }

    fn operation(&self, name: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.operations
            .write()
            .entry(name.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    pub async fn start(&self, name: &str, spec: UpstreamSpec) -> std::io::Result<()> {
        let _gate = self.shutdown_gate.read().await;
        let operation = self.operation(name);
        let _operation = operation.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("pool is shutting down"));
        }
        let existing = self.proxies.read().get(name).cloned();
        if let Some(existing) = existing {
            match existing.status() {
                crate::types::ServerStatus::Starting | crate::types::ServerStatus::Running => {
                    return existing.start().await;
                }
                _ => existing.stop().await?,
            }
        }
        let proxy = Arc::new(SocketProxy::new(
            name.to_string(),
            crate::config::server_socket_path(name),
            spec,
            true,
        ));
        // Publish before awaiting setup so status reports Starting, not absence.
        self.proxies.write().insert(name.to_string(), proxy.clone());
        if self.shutting_down.load(Ordering::SeqCst) {
            self.proxies.write().remove(name);
            return Err(std::io::Error::other("pool is shutting down"));
        }
        proxy.start().await
    }

    /// Independent backends start concurrently; each result confirms setup.
    pub async fn start_all(self: &Arc<Self>) -> std::io::Result<Vec<(String, Option<String>)>> {
        let config = crate::config::PoolConfig::load()?;
        let mut starts = tokio::task::JoinSet::new();
        let mut results = Vec::with_capacity(config.server.len());
        for (name, definition) in config.server {
            let spec = upstream_spec_from_def(&definition);
            let pool = self.clone();
            starts.spawn(async move {
                let error = pool
                    .start(&name, spec)
                    .await
                    .err()
                    .map(|error| error.to_string());
                (name, error)
            });
        }
        while let Some(result) = starts.join_next().await {
            results.push(result.map_err(std::io::Error::other)?);
        }
        results.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(results)
    }

    pub async fn stop_server(&self, name: &str) -> std::io::Result<bool> {
        let _gate = self.shutdown_gate.read().await;
        let starting = self.proxies.read().get(name).cloned();
        if let Some(starting) = starting {
            starting.request_stop();
        }
        let operation = self.operation(name);
        let _operation = operation.lock().await;
        let proxy = {
            let proxies = self.proxies.read();
            proxies.get(name).cloned()
        };

        if let Some(proxy) = proxy {
            proxy.stop().await?;
            // Remove so a subsequent start() can rebind the same socket path.
            self.proxies.write().remove(name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn restart(&self, name: &str) -> std::io::Result<bool> {
        let _gate = self.shutdown_gate.read().await;
        let operation = self.operation(name);
        let _operation = operation.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("pool is shutting down"));
        }
        let proxy = {
            let proxies = self.proxies.read();
            match proxies.get(name).cloned() {
                Some(proxy) => proxy,
                None => return Ok(false),
            }
        };

        // External (non-owned) sockets cannot be restarted by the pool.
        if !proxy.is_owned() {
            return Ok(false);
        }

        proxy.restart().await
    }

    pub async fn shutdown(&self) -> std::io::Result<()> {
        self.shutting_down.store(true, Ordering::SeqCst);
        let starting: Vec<_> = self.proxies.read().values().cloned().collect();
        for proxy in starting {
            proxy.request_stop();
        }
        let _gate = self.shutdown_gate.write().await;
        let proxies: Vec<_> = self
            .proxies
            .read()
            .iter()
            .map(|(name, proxy)| (name.clone(), proxy.clone()))
            .collect();
        let mut failure = None;
        for (name, proxy) in proxies {
            match proxy.stop().await {
                Ok(()) => {
                    self.proxies.write().remove(&name);
                }
                Err(error) => {
                    crate::diagnostics::log(format!(
                        "pool_shutdown_failed name={name} error={error}"
                    ));
                    failure = Some(error);
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// On Windows named pipes are not filesystem entries to enumerate, so there
    /// is nothing to discover. On Unix we scan the run dir for live sockets we
    /// did not start ourselves.
    pub fn discover_existing_sockets(&self) -> usize {
        if cfg!(windows) {
            return 0;
        }

        let run_dir = match crate::config::run_dir() {
            Ok(dir) => dir,
            Err(_) => return 0,
        };

        let entries = match std::fs::read_dir(&run_dir) {
            Ok(entries) => entries,
            Err(_) => return 0,
        };

        let mut discovered = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = socket_name_from_path(&path) else {
                continue;
            };

            // Skip anything already known to us; it is either running or slated.
            if self.proxies.read().contains_key(&name) {
                continue;
            }

            if !socket_alive(&path) {
                continue;
            }

            // Placeholder spec: discovered sockets are external processes we
            // attach to. transport() derives from the spec, so stdio is a safe
            // neutral choice that yields a consistent status entry.
            let placeholder = UpstreamSpec::Stdio {
                command: String::new(),
                args: Vec::new(),
                env: BTreeMap::new(),
            };
            let proxy = Arc::new(SocketProxy::new(
                name.clone(),
                path.clone(),
                placeholder,
                false,
            ));
            self.proxies.write().insert(name, proxy);
            discovered += 1;
        }

        discovered
    }

    pub fn get_status(&self) -> PoolStatusResponse {
        let proxies = self.proxies.read();
        let servers: Vec<_> = proxies
            .iter()
            .map(|(name, proxy)| crate::types::McpServerStatus {
                name: name.clone(),
                status: proxy.status(),
                socket_path: proxy.socket_path().display().to_string(),
                uptime_seconds: proxy.uptime_seconds(),
                connection_count: proxy.connection_count(),
                owned: proxy.is_owned(),
                transport: proxy.transport().to_string(),
                readiness: proxy.readiness(),
            })
            .collect();

        PoolStatusResponse {
            server_count: servers.len(),
            servers,
        }
    }

    #[cfg(test)]
    pub(crate) fn insert_test_proxy(&self, name: &str, proxy: Arc<SocketProxy>) {
        self.proxies.write().insert(name.to_string(), proxy);
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
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

/// Inverse of `crate::config::server_socket_path`: turn a run-dir entry named
/// `mcp-pool-<name>.sock` back into `<name>`. Returns None for anything that is
/// not one of our socket files.
pub fn socket_name_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_string_lossy().into_owned();
    const PREFIX: &str = "mcp-pool-";
    const SUFFIX: &str = ".sock";
    if !file_name.starts_with(PREFIX) || !file_name.ends_with(SUFFIX) {
        return None;
    }
    let trimmed = &file_name[PREFIX.len()..file_name.len() - SUFFIX.len()];
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.to_string())
}
