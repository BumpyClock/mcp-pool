use std::collections::BTreeMap;

use tokio::sync::mpsc;

/// Specification of how a pooled MCP's single upstream is driven.
#[derive(Debug, Clone)]
pub enum UpstreamSpec {
    /// Local stdio command: spawn one child, own its stdin/stdout.
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    /// Remote HTTP/SSE endpoint: hold one persistent client.
    Http { url: String, sse: bool },
}

/// Handle to a running upstream. Send JSON-RPC request lines on `request_tx`;
/// the upstream emits response lines on the `response_tx` it was spawned with.
pub struct UpstreamHandle {
    pub request_tx: mpsc::Sender<String>,
}

impl UpstreamHandle {
    pub async fn spawn(
        spec: UpstreamSpec,
        response_tx: mpsc::Sender<String>,
    ) -> std::io::Result<UpstreamHandle> {
        match spec {
            UpstreamSpec::Stdio {
                command,
                args,
                env,
            } => Self::spawn_stdio(command, args, env, response_tx).await,
            UpstreamSpec::Http { url, sse } => Self::spawn_http(url, sse, response_tx).await,
        }
    }

    async fn spawn_stdio(
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        response_tx: mpsc::Sender<String>,
    ) -> std::io::Result<UpstreamHandle> {
        let _ = (command, args, env, response_tx);
        todo!("spawn child: stdin writer <- request_tx, stdout reader -> response_tx (newline-delimited JSON-RPC)")
    }

    async fn spawn_http(
        url: String,
        sse: bool,
        response_tx: mpsc::Sender<String>,
    ) -> std::io::Result<UpstreamHandle> {
        let _ = (url, sse, response_tx);
        todo!("http/sse upstream: per-request POST, route JSON or SSE data lines -> response_tx")
    }
}
