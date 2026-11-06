use std::io;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;

use crate::config::ConfigurationEntry;
use crate::socket_proxy::SocketProxy;
#[cfg(unix)]
use crate::types::ServerReadiness;
use crate::types::{McpServerStatus, ServerStatus};

#[derive(Clone)]
pub(super) enum Endpoint {
    Owned(Arc<SocketProxy>),
    #[cfg(unix)]
    External(PathBuf),
}

impl Endpoint {
    pub(super) fn configuration_entry(&self) -> Option<&ConfigurationEntry> {
        match self {
            Self::Owned(proxy) => proxy.configuration_entry(),
            #[cfg(unix)]
            Self::External(_) => None,
        }
    }

    pub(super) fn status(&self) -> ServerStatus {
        match self {
            Self::Owned(proxy) => proxy.status(),
            #[cfg(unix)]
            Self::External(_) => ServerStatus::Running,
        }
    }

    pub(super) fn request_stop(&self) {
        match self {
            Self::Owned(proxy) => proxy.request_stop(),
            #[cfg(unix)]
            Self::External(_) => {}
        }
    }

    pub(super) async fn start(&self) -> io::Result<()> {
        match self {
            Self::Owned(proxy) => proxy.start().await,
            #[cfg(unix)]
            Self::External(_) => Ok(()),
        }
    }

    pub(super) async fn stop(&self) -> io::Result<()> {
        match self {
            Self::Owned(proxy) => proxy.stop().await,
            #[cfg(unix)]
            Self::External(_) => Ok(()),
        }
    }

    pub(super) async fn restart(&self) -> io::Result<()> {
        match self {
            Self::Owned(proxy) => proxy.restart().await,
            #[cfg(unix)]
            Self::External(_) => Ok(()),
        }
    }

    pub(super) fn wire_status(&self, name: &str) -> McpServerStatus {
        match self {
            Self::Owned(proxy) => McpServerStatus {
                name: name.to_owned(),
                status: proxy.status(),
                socket_path: proxy.socket_path().display().to_string(),
                uptime_seconds: proxy.uptime_seconds(),
                connection_count: proxy.connection_count(),
                owned: true,
                transport: proxy.transport().to_string(),
                readiness: proxy.readiness(),
                configuration_entry: proxy.configuration_entry().cloned(),
            },
            #[cfg(unix)]
            Self::External(path) => McpServerStatus {
                name: name.to_owned(),
                status: ServerStatus::Running,
                socket_path: path.display().to_string(),
                uptime_seconds: None,
                connection_count: 0,
                owned: false,
                // Discovery cannot identify the remote transport. Retain the wire default.
                transport: "stdio".to_owned(),
                readiness: ServerReadiness {
                    local_socket_bound: true,
                    ..ServerReadiness::default()
                },
                configuration_entry: None,
            },
        }
    }
}
