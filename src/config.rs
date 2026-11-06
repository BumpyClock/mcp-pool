use std::collections::BTreeMap;
use std::io;
#[cfg(any(unix, test))]
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub fn state_dir() -> io::Result<PathBuf> {
    if let Some(custom) = crate::environment::value("MCP_CLI_HOME") {
        return Ok(PathBuf::from(custom).join("state"));
    }
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    Ok(home.join(".mcp"))
}

#[cfg(unix)]
pub fn run_dir() -> io::Result<PathBuf> {
    Ok(state_dir()?.join("run"))
}

/// Windows named pipes use a stable per-home hash because their namespace is global.
#[cfg(windows)]
fn home_scope_hash() -> io::Result<String> {
    let seed = crate::environment::string("MCP_CLI_HOME")
        .or_else(|| dirs::home_dir().map(|path| path.to_string_lossy().into_owned()))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in seed.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    Ok(format!("{:08x}", hash as u32))
}

#[cfg(unix)]
fn unix_socket_path(directory: io::Result<PathBuf>, filename: &str) -> io::Result<PathBuf> {
    Ok(directory?.join(filename))
}

pub fn control_socket_path() -> io::Result<PathBuf> {
    #[cfg(unix)]
    {
        unix_socket_path(state_dir(), "mcp-cli-control.sock")
    }
    #[cfg(windows)]
    {
        let scope = home_scope_hash()?;
        Ok(PathBuf::from(format!(r"\\.\pipe\mcp-cli-{scope}-control")))
    }
}

pub fn server_socket_path(name: &str) -> io::Result<PathBuf> {
    let safe = sanitize_socket_name(name);
    #[cfg(unix)]
    {
        unix_socket_path(run_dir(), &format!("mcp-cli-{safe}.sock"))
    }
    #[cfg(windows)]
    {
        let scope = home_scope_hash()?;
        Ok(PathBuf::from(format!(r"\\.\pipe\mcp-cli-{scope}-{safe}")))
    }
}

#[cfg(any(unix, test))]
pub fn server_name_from_socket_path(path: &Path) -> Option<String> {
    let filename = path.file_name()?.to_string_lossy();
    filename
        .strip_prefix("mcp-cli-")?
        .strip_suffix(".sock")
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
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

    #[tokio::test]
    async fn state_directory_defaults_to_mcp_home_and_preserves_overrides() -> io::Result<()> {
        let home = dirs::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
        for (custom, expected) in [
            (None, home.join(".mcp")),
            (
                Some("custom-pool-home"),
                PathBuf::from("custom-pool-home/state"),
            ),
        ] {
            let mut command = tokio::process::Command::new(std::env::current_exe()?);
            command
                .args([
                    "--exact",
                    "config::tests::state_directory_fixture",
                    "--nocapture",
                ])
                .env("MCP_CLI_EXPECTED_STATE", expected)
                .env_remove("MCP_CLI_HOME")
                .env_remove("MCP_POOL_HOME");
            if let Some(custom) = custom {
                command.env("MCP_CLI_HOME", custom);
            }
            let output = command.output().await?;
            assert!(output.status.success(), "{output:?}");
        }
        Ok(())
    }

    #[test]
    fn state_directory_fixture() -> io::Result<()> {
        let Some(expected) = std::env::var_os("MCP_CLI_EXPECTED_STATE") else {
            return Ok(());
        };
        let expected = PathBuf::from(expected);
        assert_eq!(state_dir()?, expected);
        #[cfg(unix)]
        {
            assert_eq!(run_dir()?, expected.join("run"));
            assert_eq!(
                control_socket_path()?,
                expected.join("mcp-cli-control.sock")
            );
            assert_eq!(
                server_socket_path("fixture")?,
                expected.join("run/mcp-cli-fixture.sock")
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_resolution_preserves_state_directory_errors() {
        let error = unix_socket_path(
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "state directory unavailable",
            )),
            "mcp-cli-control.sock",
        )
        .err();
        assert_eq!(
            error.as_ref().map(io::Error::kind),
            Some(io::ErrorKind::NotFound)
        );
        assert_eq!(
            error.map(|error| error.to_string()).as_deref(),
            Some("state directory unavailable")
        );
    }

    #[cfg(unix)]
    #[test]
    fn endpoint_codec_round_trips_the_sanitized_endpoint_name() -> io::Result<()> {
        for (name, expected) in [
            ("echo", "echo"),
            ("a.b", "a_b"),
            ("", "mcp"),
            ("écho", "_cho"),
        ] {
            assert_eq!(
                server_name_from_socket_path(&server_socket_path(name)?).as_deref(),
                Some(expected)
            );
        }
        Ok(())
    }

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
