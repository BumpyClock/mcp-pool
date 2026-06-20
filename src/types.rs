use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServerStatus {
    Stopped,
    Starting,
    Running,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerStatus {
    pub name: String,
    pub status: String,
    pub socket_path: String,
    pub uptime_seconds: Option<u64>,
    pub connection_count: u32,
    pub owned: bool,
    pub transport: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PoolStatusResponse {
    pub server_count: usize,
    pub servers: Vec<McpServerStatus>,
}
