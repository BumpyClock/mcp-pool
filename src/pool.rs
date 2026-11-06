#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::HashMap;
#[cfg(any(unix, test))]
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::RwLock;

use crate::config::ServerDef;
use crate::socket_proxy::SocketProxy;
use crate::types::PoolStatusResponse;
use crate::upstream::UpstreamSpec;

#[path = "pool_endpoint.rs"]
mod endpoint;
use endpoint::Endpoint;

pub struct Pool {
    proxies: RwLock<HashMap<String, Endpoint>>,
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
            crate::config::server_socket_path(name)?,
            spec,
            configuration_entry,
        ));
        self.proxies
            .write()
            .insert(name.to_string(), Endpoint::Owned(proxy.clone()));
        if self.shutting_down.load(Ordering::SeqCst) {
            self.proxies.write().remove(name);
            return Err(std::io::Error::other("pool is shutting down"));
        }
        proxy.start().await
    }

    pub async fn stop_server(&self, name: &str) -> std::io::Result<()> {
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
        }
        Ok(())
    }

    pub async fn restart(&self, name: &str) -> std::io::Result<()> {
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
                None => return Ok(()),
            }
        };

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
        for (_, proxy) in &proxies {
            proxy.request_stop();
        }
        let mut failure = None;
        for (name, proxy) in proxies {
            match proxy.stop().await {
                Ok(()) => {
                    self.proxies.write().remove(&name);
                }
                Err(error) => {
                    crate::diagnostics::ServerLog::for_runtime(&name)
                        .log(format!("pool_shutdown_failed error={error}"));
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
    #[cfg(unix)]
    pub fn discover_existing_sockets(&self) -> std::io::Result<usize> {
        self.discover_sockets_in(&crate::config::run_dir()?)
    }

    #[cfg(unix)]
    fn discover_sockets_in(&self, directory: &Path) -> std::io::Result<usize> {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let mut discovered = 0;
        for entry in entries {
            let path = entry?.path();
            let Some(name) = crate::config::server_name_from_socket_path(&path) else {
                continue;
            };
            if self.proxies.read().contains_key(&name) || !socket_alive(&path) {
                continue;
            }
            self.proxies.write().insert(name, Endpoint::External(path));
            discovered += 1;
        }
        Ok(discovered)
    }

    #[cfg(windows)]
    pub fn discover_existing_sockets(&self) -> std::io::Result<usize> {
        Ok(0)
    }

    pub fn get_status(&self) -> PoolStatusResponse {
        let proxies = self.proxies.read();
        let servers: Vec<_> = proxies
            .iter()
            .map(|(name, endpoint)| endpoint.wire_status(name))
            .collect();

        PoolStatusResponse {
            server_count: servers.len(),
            servers,
        }
    }

    #[cfg(test)]
    pub(crate) fn insert_test_proxy(&self, name: &str, proxy: Arc<SocketProxy>) {
        self.proxies
            .write()
            .insert(name.to_string(), Endpoint::Owned(proxy));
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

#[cfg(unix)]
pub fn socket_alive(path: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(path).is_ok()
}

#[cfg(test)]
mod options_tests {
    use super::*;

    #[test]
    fn socket_names_require_the_complete_nonempty_endpoint_pattern() {
        for (filename, expected) in [
            ("mcp-cli-echo.sock", Some("echo")),
            ("mcp-cli-name.sock.sock", Some("name.sock")),
            ("mcp-cli-écho.sock", Some("écho")),
            ("mcp-cli-.sock", None),
            ("mcp-cli-echo", None),
            ("other-echo.sock", None),
            ("mcp-cli-echo.sock.backup", None),
        ] {
            assert_eq!(
                crate::config::server_name_from_socket_path(Path::new(filename)).as_deref(),
                expected,
                "{filename}"
            );
        }
    }

    #[tokio::test]
    async fn missing_lifecycle_operations_remain_noops() -> std::io::Result<()> {
        let pool = Pool::new();
        pool.stop_server("missing").await?;
        pool.restart("missing").await?;
        assert_eq!(pool.get_status().server_count, 0);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn borrowed_lifecycle_operations_remain_noops() -> std::io::Result<()> {
        let pool = Pool::new();
        pool.proxies.write().insert(
            "borrowed".into(),
            Endpoint::External("borrowed-endpoint".into()),
        );
        pool.restart("borrowed").await?;
        let status = pool.get_status();
        let server = status
            .servers
            .first()
            .ok_or_else(|| std::io::Error::other("missing borrowed server"))?;
        assert_eq!(server.status, crate::types::ServerStatus::Running);
        assert!(!server.owned);
        assert_eq!(server.socket_path, "borrowed-endpoint");
        pool.stop_server("borrowed").await?;
        assert_eq!(pool.get_status().server_count, 0);
        Ok(())
    }

    #[test]
    fn status_preserves_configuration_entry_ownership() -> std::io::Result<()> {
        let entry = crate::config::ConfigurationEntry {
            source: "fixture-source.json".into(),
            name: "configured-server".into(),
            scope: None,
            credential_store: None,
        };
        let pool = Pool::new();
        pool.insert_test_proxy(
            "resolved-runtime",
            Arc::new(SocketProxy::new(
                "resolved-runtime".into(),
                "unused-fixture-endpoint".into(),
                upstream_spec_from_def(&ServerDef::default()),
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
            scope: None,
            credential_store: None,
        };
        let pool = Pool::new();
        pool.insert_test_proxy(
            "resolved-runtime",
            Arc::new(SocketProxy::new(
                "resolved-runtime".into(),
                "unused-fixture-endpoint".into(),
                upstream_spec_from_def(&ServerDef::default()),
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

    #[cfg(unix)]
    #[tokio::test]
    async fn discovery_preserves_external_listener_ownership_through_lifecycle_operations()
    -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let directory = std::env::temp_dir().join(format!("mcpe-{}", std::process::id()));
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let path = directory.join("mcp-cli-external.sock");
        let listener = crate::transport::bind(&path)?;
        let pool = Pool::new();
        assert_eq!(pool.discover_sockets_in(&directory)?, 1);
        assert_eq!(pool.discover_sockets_in(&directory)?, 0);
        let status = pool.get_status();
        let server = status
            .servers
            .first()
            .ok_or_else(|| std::io::Error::other("missing external endpoint"))?;
        assert!(!server.owned);
        assert_eq!(server.transport, "stdio");
        assert!(server.readiness.local_socket_bound);
        assert!(!server.readiness.upstream_transport_ready);
        assert!(server.configuration_entry.is_none());
        pool.restart("external").await?;
        pool.stop_server("external").await?;
        assert_eq!(pool.get_status().server_count, 0);
        assert!(socket_alive(&path));
        assert_eq!(pool.discover_sockets_in(&directory)?, 1);
        pool.shutdown().await?;
        assert!(socket_alive(&path));
        drop(listener);
        std::fs::remove_file(path)?;
        std::fs::remove_dir(directory)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn discovery_treats_only_a_missing_directory_as_empty() -> std::io::Result<()> {
        let directory = std::env::temp_dir().join(format!("mcp-scan-{}", std::process::id()));
        let pool = Pool::new();
        assert_eq!(pool.discover_sockets_in(&directory)?, 0);
        std::fs::write(&directory, b"not a directory")?;
        let result = pool.discover_sockets_in(&directory);
        std::fs::remove_file(directory)?;
        assert!(result.is_err());
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
