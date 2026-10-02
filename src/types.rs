use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerReadiness {
    pub local_socket_bound: bool,
    pub upstream_transport_ready: bool,
    pub mcp_initialize_result_received: bool,
    pub startup_error: Option<String>,
    pub retirement_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    pub status: ServerStatus,
    pub socket_path: String,
    pub uptime_seconds: Option<u64>,
    pub connection_count: u32,
    pub owned: bool,
    pub transport: String,
    pub readiness: ServerReadiness,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PoolStatusResponse {
    pub server_count: usize,
    pub servers: Vec<McpServerStatus>,
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_string(&ServerStatus::Running).unwrap(),
            "\"running\""
        );
        assert_eq!(
            serde_json::to_string(&ServerStatus::Starting).unwrap(),
            "\"starting\""
        );
        assert_eq!(
            serde_json::to_string(&ServerStatus::Stopped).unwrap(),
            "\"stopped\""
        );
    }
}
