use serde::{Deserialize, Serialize};

/// Request envelope sent by the CLI to the daemon over the control socket.
/// One JSON object per line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum ControlRequest {
    Start { name: String },
    Stop { name: String },
    Restart { name: String },
    Status { name: Option<String> },
    List,
    Shutdown,
}

/// Response envelope returned by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl ControlResponse {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            data: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            data: None,
        }
    }

    pub fn data(value: serde_json::Value) -> Self {
        Self {
            ok: true,
            error: None,
            data: Some(value),
        }
    }
}
