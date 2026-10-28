use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

const ENV_HOME: &str = "MCP_POOL_HOME";

pub fn state_dir() -> io::Result<PathBuf> {
    if let Ok(custom) = std::env::var(ENV_HOME) {
        return Ok(PathBuf::from(custom).join("state"));
    }
    let base = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "state directory not found"))?;
    Ok(base.join("mcp-pool"))
}

#[cfg(unix)]
pub fn run_dir() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("run"))
}

/// Windows named pipes use a stable per-home hash because their namespace is global.
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

#[cfg(unix)]
fn unix_socket_path(resolved: io::Result<PathBuf>, file_name: &str) -> PathBuf {
    resolved
        .map(|dir| dir.join(file_name))
        .unwrap_or_else(|_| PathBuf::from(format!("/tmp/{file_name}")))
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigurationEntry {
    pub source: PathBuf,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_store: Option<PathBuf>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct ServerDef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential_scope: Option<PathBuf>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_entry: Option<ConfigurationEntry>,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub command: String,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_env: bool,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,

    #[serde(
        default,
        skip_serializing_if = "crate::mcp_protocol::ProtocolPreference::is_auto"
    )]
    pub protocol_version: crate::mcp_protocol::ProtocolPreference,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<crate::oauth::HttpAuth>,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub transport: String,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl std::fmt::Debug for ServerDef {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServerDef")
            .field("transport", &self.transport_kind())
            .field("timeout_ms", &self.timeout_ms)
            .field("protocol_version", &self.protocol_version)
            .finish_non_exhaustive()
    }
}

impl ServerDef {
    pub fn is_remote(&self) -> bool {
        !self.url.is_empty()
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_def_transport_kind() {
        let stdio = ServerDef {
            command: "npx".into(),
            ..Default::default()
        };
        assert_eq!(stdio.transport_kind(), "stdio");
        let http = ServerDef {
            url: "http://x".into(),
            ..Default::default()
        };
        assert_eq!(http.transport_kind(), "http");
        let sse = ServerDef {
            url: "http://x".into(),
            transport: "sse".into(),
            ..Default::default()
        };
        assert_eq!(sse.transport_kind(), "sse");
    }

    #[test]
    fn server_debug_omits_connection_secrets() {
        let definition = ServerDef {
            url: "https://example.invalid/mcp?secret=fixture-secret".into(),
            headers: BTreeMap::from([("Authorization".into(), "fixture-secret".into())]),
            env: BTreeMap::from([("API_KEY".into(), "fixture-secret".into())]),
            ..Default::default()
        };
        let formatted = format!("{definition:?}");
        assert!(formatted.contains("http"));
        assert!(!formatted.contains("fixture-secret"));
        assert!(!formatted.contains("example.invalid"));
    }
}
