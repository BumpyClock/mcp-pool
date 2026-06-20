use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use parking_lot::RwLock;

use crate::config::ServerDef;
use crate::socket_proxy::SocketProxy;
use crate::types::{PoolStatusResponse, ServerStatus};
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

    pub fn start(&self, name: &str, spec: UpstreamSpec) -> std::io::Result<()> {
        // Hold the write lock across the whole check-bind-insert so concurrent
        // auto-starts (every proxy now self-starts) cannot both pass the liveness
        // check and race to bind the same socket. parking_lot is not reentrant, so
        // the liveness check is inlined here rather than factored into a helper
        // that would need to re-acquire the lock.
        let mut proxies = self.proxies.write();
        if let Some(existing) = proxies.get(name)
            && existing.status() == ServerStatus::Running {
                // Trust the status for an owned proxy: its upstream task flips the
                // status to Stopped on exit. Do NOT probe with socket_alive here —
                // that is a blocking pipe connect held under the write lock, which
                // can stall the whole pool when a server is busy (e.g. mid-auth),
                // and it spawns a phantom client connection on every re-start.
                return Ok(());
            }

        let socket_path = crate::config::server_socket_path(name);
        let proxy = Arc::new(SocketProxy::new(
            name.to_string(),
            socket_path,
            spec,
            true,
        ));
        proxy.start()?;

        proxies.insert(name.to_string(), proxy);
        Ok(())
    }

    /// Start every configured server. Each call to `start()` returns as soon as the
    /// socket is bound and the upstream task is spawned, so the child processes boot
    /// concurrently in their own background tasks rather than one-at-a-time. Returns
    /// one (name, optional error string) per configured server, preserving the
    /// outcome of each so a single bad entry does not abort the rest.
    pub fn start_all(&self) -> std::io::Result<Vec<(String, Option<String>)>> {
        let config = crate::config::PoolConfig::load()?;
        let mut results = Vec::with_capacity(config.server.len());
        for (name, definition) in &config.server {
            let spec = upstream_spec_from_def(definition);
            let error = self.start(name, spec).err().map(|error| error.to_string());
            results.push((name.clone(), error));
        }
        Ok(results)
    }

    pub fn stop_server(&self, name: &str) -> std::io::Result<bool> {
        let proxy = {
            let proxies = self.proxies.read();
            proxies.get(name).cloned()
        };

        if let Some(proxy) = proxy {
            proxy.stop()?;
            // Remove so a subsequent start() can rebind the same socket path.
            self.proxies.write().remove(name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn restart(&self, name: &str) -> std::io::Result<bool> {
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

    pub fn shutdown(&self) {
        let mut proxies = self.proxies.write();
        for proxy in proxies.values() {
            // Stopping is best-effort during shutdown; keep going regardless.
            if let Err(error) = proxy.stop() {
                eprintln!("pool shutdown stop failed: {error}");
            }
        }
        proxies.clear();
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
            // start() on a non-owned proxy just marks it Running without
            // spawning an upstream.
            if let Err(error) = proxy.start() {
                eprintln!("pool discover start failed for {name}: {error}");
                continue;
            }

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
            })
            .collect();

        PoolStatusResponse {
            server_count: servers.len(),
            servers,
        }
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
