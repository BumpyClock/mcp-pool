use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;

use crate::config::ServerDef;
use crate::socket_proxy::SocketProxy;
use crate::types::PoolStatusResponse;
use crate::upstream::UpstreamSpec;

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

    pub async fn start(
        &self,
        name: &str,
        spec: UpstreamSpec,
        configuration_entry: Option<crate::config::ConfigurationEntry>,
    ) -> std::io::Result<()> {
        let _gate = self.shutdown_gate.read().await;
        let operation = self.operation(name);
        let _operation = operation.lock().await;
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err(std::io::Error::other("pool is shutting down"));
        }
        let existing = self.proxies.read().get(name).cloned();
        if let Some(existing) = &existing
            && existing.configuration_entry() != configuration_entry.as_ref()
        {
            return Err(std::io::Error::other(
                "pool belongs to a different configuration entry",
            ));
        }
        crate::diagnostics::logging::register_identity(
            name,
            configuration_entry
                .as_ref()
                .map_or(name, |entry| entry.name.as_str()),
        )
        .map_err(std::io::Error::other)?;
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
            configuration_entry,
        ));
        self.proxies.write().insert(name.to_string(), proxy.clone());
        if self.shutting_down.load(Ordering::SeqCst) {
            self.proxies.write().remove(name);
            return Err(std::io::Error::other("pool is shutting down"));
        }
        proxy.start().await
    }

    pub async fn start_all(self: &Arc<Self>) -> std::io::Result<Vec<(String, Option<String>)>> {
        let config = crate::config::PoolConfig::load()?;
        let mut starts = tokio::task::JoinSet::new();
        let mut results = Vec::with_capacity(config.server.len());
        for (name, definition) in config.server {
            let spec = upstream_spec_from_def(&definition);
            let pool = self.clone();
            starts.spawn(async move {
                let error = pool
                    .start(&name, spec, None)
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
            Some(error) => {
                self.shutting_down.store(false, Ordering::SeqCst);
                Err(error)
            }
            None => Ok(()),
        }
    }

    /// Registers live Unix sockets without taking upstream ownership.
    /// Windows named pipes cannot be enumerated.
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

            if self.proxies.read().contains_key(&name) {
                continue;
            }

            if !socket_alive(&path) {
                continue;
            }

            let placeholder = UpstreamSpec::Stdio {
                command: String::new(),
                args: Vec::new(),
                env: BTreeMap::new(),
                cwd: None,
                clear_env: false,
            };
            let proxy = Arc::new(SocketProxy::new(
                name.clone(),
                path.clone(),
                placeholder,
                false,
                None,
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
                configuration_entry: proxy.configuration_entry().cloned(),
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

pub fn upstream_spec_from_def(def: &ServerDef) -> UpstreamSpec {
    if def.is_remote() {
        UpstreamSpec::Http {
            url: def.url.clone(),
            sse: def.transport.eq_ignore_ascii_case("sse"),
            headers: def.headers.clone(),
            timeout_ms: def.timeout_ms,
            auth: def.auth.clone().map(Box::new),
        }
    } else {
        UpstreamSpec::Stdio {
            command: def.command.clone(),
            args: def.args.clone(),
            env: def.env.clone(),
            cwd: def.cwd.clone(),
            clear_env: def.clear_env,
        }
    }
}

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

pub fn socket_name_from_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_string_lossy();
    file_name
        .strip_prefix("mcp-pool-")?
        .strip_suffix(".sock")
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod options_tests {
    use super::*;

    #[test]
    fn socket_names_require_the_complete_nonempty_endpoint_pattern() {
        for (filename, expected) in [
            ("mcp-pool-echo.sock", Some("echo")),
            ("mcp-pool-name.sock.sock", Some("name.sock")),
            ("mcp-pool-écho.sock", Some("écho")),
            ("mcp-pool-.sock", None),
            ("mcp-pool-echo", None),
            ("other-echo.sock", None),
            ("mcp-pool-echo.sock.backup", None),
        ] {
            assert_eq!(
                socket_name_from_path(Path::new(filename)).as_deref(),
                expected,
                "{filename}"
            );
        }
    }

    #[test]
    fn status_preserves_configuration_entry_ownership() -> std::io::Result<()> {
        let entry = crate::config::ConfigurationEntry {
            source: "fixture-source.json".into(),
            name: "configured-server".into(),
        };
        let pool = Pool::new();
        pool.insert_test_proxy(
            "resolved-runtime",
            Arc::new(SocketProxy::new(
                "resolved-runtime".into(),
                "unused-fixture-endpoint".into(),
                upstream_spec_from_def(&ServerDef::default()),
                true,
                Some(entry.clone()),
            )),
        );
        let status = pool.get_status();
        let server = status
            .servers
            .first()
            .ok_or_else(|| std::io::Error::other("missing registered server"))?;
        assert_eq!(server.configuration_entry.as_ref(), Some(&entry));
        let serialized = serde_json::to_value(server)?;
        assert_eq!(
            serialized.get("configuration_entry"),
            Some(&serde_json::json!({
                "source":"fixture-source.json", "name":"configured-server"
            }))
        );
        Ok(())
    }

    #[tokio::test]
    async fn an_existing_runtime_cannot_be_claimed_by_another_entry() -> std::io::Result<()> {
        let entry = crate::config::ConfigurationEntry {
            source: "fixture-source.json".into(),
            name: "configured-server".into(),
        };
        let pool = Pool::new();
        pool.insert_test_proxy(
            "resolved-runtime",
            Arc::new(SocketProxy::new(
                "resolved-runtime".into(),
                "unused-fixture-endpoint".into(),
                upstream_spec_from_def(&ServerDef::default()),
                true,
                Some(entry.clone()),
            )),
        );
        let replacement = crate::config::ConfigurationEntry {
            name: "another-server".into(),
            ..entry.clone()
        };
        let result = pool
            .start(
                "resolved-runtime",
                upstream_spec_from_def(&ServerDef::default()),
                Some(replacement),
            )
            .await;
        assert!(result.is_err());
        let status = pool.get_status();
        assert_eq!(status.server_count, 1);
        assert_eq!(
            status
                .servers
                .first()
                .and_then(|server| server.configuration_entry.as_ref()),
            Some(&entry)
        );
        Ok(())
    }

    #[test]
    fn server_definition_preserves_transport_options() {
        let definition = ServerDef {
            command: "synthetic-command".into(),
            args: vec!["synthetic-argument".into()],
            env: BTreeMap::from([("SYNTHETIC_ENV".into(), "synthetic-value".into())]),
            cwd: Some(std::path::PathBuf::from("synthetic-cwd")),
            ..Default::default()
        };
        let stdio = upstream_spec_from_def(&definition);
        assert!(
            matches!(&stdio, UpstreamSpec::Stdio { command, args, env, cwd, clear_env: false }
            if command == &definition.command && args == &definition.args
            && env == &definition.env && cwd == &definition.cwd)
        );
        let captured = ServerDef {
            clear_env: true,
            ..definition
        };
        assert!(matches!(
            upstream_spec_from_def(&captured),
            UpstreamSpec::Stdio {
                clear_env: true,
                ..
            }
        ));
        let remote = ServerDef {
            url: "http://127.0.0.1:1/synthetic".into(),
            transport: "SSE".into(),
            headers: BTreeMap::from([("Authorization".into(), "synthetic-secret".into())]),
            timeout_ms: Some(120_000),
            ..Default::default()
        };
        let http = upstream_spec_from_def(&remote);
        assert!(
            matches!(&http, UpstreamSpec::Http { url, sse: true, headers, timeout_ms: Some(120_000), auth: None }
            if url == &remote.url && headers == &remote.headers)
        );
        assert!(!format!("{http:?}").contains("synthetic-secret"));
    }
}
