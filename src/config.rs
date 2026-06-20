use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Override the entire mcp-pool home (config + state) for tests/isolation.
const ENV_HOME: &str = "MCP_POOL_HOME";

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
        .or_else(dirs::data_local_dir)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "state directory not found"))?;
    Ok(base.join("mcp-pool"))
}

pub fn run_dir() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("run"))
}

pub fn config_path() -> io::Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Stable short hash of the effective home identity, used to namespace Windows
/// named pipes. Unix socket paths live under `state_dir()` (which already honors
/// `MCP_POOL_HOME`), but Windows pipe names are a machine-global namespace, so a
/// fixed name would let distinct homes — parallel test pools, or two users —
/// collide on the same pipe and even attach to the wrong daemon. The daemon and
/// CLI are the same binary reading the same environment, so both derive the same
/// value. FNV-1a (rather than DefaultHasher) keeps the result explicit and stable
/// across processes regardless of std internals.
#[cfg(windows)]
fn home_scope_hash() -> String {
    let seed = std::env::var(ENV_HOME)
        .ok()
        .or_else(|| dirs::home_dir().map(|path| path.to_string_lossy().into_owned()))
        .unwrap_or_default();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:08x}", hash as u32)
}

/// Resolve a Unix socket path under `resolved` (the state/run dir) or fall back to
/// /tmp when directory resolution fails. Single owner of the fallback scheme so the
/// two socket-path builders cannot drift; the primary and fallback share one
/// `file_name`, keeping the path identity stable regardless of which branch wins.
#[cfg(unix)]
fn unix_socket_path(resolved: io::Result<PathBuf>, file_name: &str) -> PathBuf {
    resolved
        .map(|dir| dir.join(file_name))
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/{file_name}")))
}

/// Control socket path. Unix: a socket file in the state dir.
/// Windows: a named-pipe name, namespaced by the home hash so isolated homes do
/// not share one global control pipe.
pub fn control_socket_path() -> PathBuf {
    #[cfg(unix)]
    {
        unix_socket_path(state_dir(), "mcp-pool-control.sock")
    }
    #[cfg(windows)]
    {
        let scope = home_scope_hash();
        PathBuf::from(format!(r"\\.\pipe\mcp-pool-{scope}-control"))
    }
}

/// Per-server pool socket path (Unix socket file / Windows named-pipe name).
pub fn server_socket_path(name: &str) -> PathBuf {
    let safe = sanitize_socket_name(name);
    #[cfg(unix)]
    {
        unix_socket_path(run_dir(), &format!("mcp-pool-{safe}.sock"))
    }
    #[cfg(windows)]
    {
        let scope = home_scope_hash();
        PathBuf::from(format!(r"\\.\pipe\mcp-pool-{scope}-{safe}"))
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
            .map_err(|error| io::Error::other(error.to_string()))?;
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
