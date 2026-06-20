use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Override the entire mcp-pool home (config + state) for tests/isolation.
const ENV_HOME: &str = "MCP_POOL_HOME";

#[allow(dead_code)]
pub fn home_dir() -> io::Result<PathBuf> {
    if let Ok(custom) = std::env::var(ENV_HOME) {
        return Ok(PathBuf::from(custom));
    }
    dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))
}

pub fn config_dir() -> io::Result<PathBuf> {
    if let Ok(custom) = std::env::var(ENV_HOME) {
        return Ok(PathBuf::from(custom).join("config"));
    }
    let base = dirs::config_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "config directory not found"))?;
    Ok(base.join("mcp-pool"))
}

pub fn state_dir() -> io::Result<PathBuf> {
    if let Ok(custom) = std::env::var(ENV_HOME) {
        return Ok(PathBuf::from(custom).join("state"));
    }
    // dirs::state_dir() is None on Windows; fall back to the local data dir.
    let base = dirs::state_dir()
        .or_else(|| dirs::data_local_dir())
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "state directory not found"))?;
    Ok(base.join("mcp-pool"))
}

pub fn run_dir() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("run"))
}

pub fn config_path() -> io::Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Control socket path. Unix: a socket file in the state dir.
/// Windows: a named-pipe name expressed as a path.
pub fn control_socket_path() -> PathBuf {
    #[cfg(unix)]
    {
        state_dir()
            .map(|dir| dir.join("control.sock"))
            .unwrap_or_else(|_| PathBuf::from("/tmp/mcp-pool-control.sock"))
    }
    #[cfg(windows)]
    {
        PathBuf::from(r"\\.\pipe\mcp-pool-control")
    }
}

/// Per-server pool socket path (Unix socket file / Windows named-pipe name).
pub fn server_socket_path(name: &str) -> PathBuf {
    let safe = sanitize_socket_name(name);
    #[cfg(unix)]
    {
        run_dir()
            .map(|dir| dir.join(format!("mcp-pool-{safe}.sock")))
            .unwrap_or_else(|_| PathBuf::from(format!("/tmp/mcp-pool-{safe}.sock")))
    }
    #[cfg(windows)]
    {
        PathBuf::from(format!(r"\\.\pipe\mcp-pool-{safe}"))
    }
}

fn sanitize_socket_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for character in name.chars() {
        if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
            out.push(character);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "mcp".to_string()
    } else {
        out
    }
}

/// A configured MCP server. Either a local stdio command or a remote HTTP/SSE URL.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerDef {
    /// Executable to run for stdio MCPs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,

    /// Arguments for the stdio command.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    /// Environment variables for the stdio command.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,

    /// URL for HTTP/SSE MCPs.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,

    /// Remote transport: "http" or "sse". Ignored for stdio.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transport: String,

    /// Human-readable description.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl ServerDef {
    pub fn is_remote(&self) -> bool {
        !self.url.is_empty()
    }

    /// Effective transport: "stdio" | "http" | "sse".
    pub fn transport_kind(&self) -> &'static str {
        if self.is_remote() {
            if self.transport.eq_ignore_ascii_case("sse") {
                "sse"
            } else {
                "http"
            }
        } else {
            "stdio"
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PoolConfig {
    #[serde(default)]
    pub server: BTreeMap<String, ServerDef>,
}

impl PoolConfig {
    pub fn load() -> io::Result<PoolConfig> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(PoolConfig::default());
        }
        let contents = std::fs::read_to_string(&path)?;
        toml::from_str(&contents).map_err(|error| {
            io::Error::new(io::ErrorKind::InvalidData, format!("{}: {error}", path.display()))
        })
    }

    pub fn save(&self) -> io::Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let serialized = toml::to_string_pretty(self)
            .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
        // Atomic write: temp file + rename.
        let temp = path.with_extension("toml.tmp");
        std::fs::write(&temp, serialized)?;
        std::fs::rename(&temp, &path)?;
        Ok(())
    }

    pub fn upsert(&mut self, name: &str, def: ServerDef) {
        self.server.insert(name.to_string(), def);
    }

    pub fn remove(&mut self, name: &str) -> bool {
        self.server.remove(name).is_some()
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_def_transport_kind() {
        let stdio = ServerDef { command: "npx".into(), ..Default::default() };
        assert_eq!(stdio.transport_kind(), "stdio");
        let http = ServerDef { url: "http://x".into(), ..Default::default() };
        assert_eq!(http.transport_kind(), "http");
        let sse = ServerDef { url: "http://x".into(), transport: "sse".into(), ..Default::default() };
        assert_eq!(sse.transport_kind(), "sse");
    }

    #[test]
    fn pool_config_toml_round_trip() {
        let mut cfg = PoolConfig::default();
        cfg.upsert(
            "echo",
            ServerDef { command: "npx".into(), args: vec!["-y".into()], ..Default::default() },
        );
        let serialized = toml::to_string(&cfg).unwrap();
        let mut back: PoolConfig = toml::from_str(&serialized).unwrap();
        assert_eq!(back.server.len(), 1);
        assert_eq!(back.server["echo"].command, "npx");
        assert_eq!(back.server["echo"].args, vec!["-y".to_string()]);
        assert!(back.remove("echo"));
        assert!(!back.remove("echo"));
    }
}
