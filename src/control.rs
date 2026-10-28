use serde::{Deserialize, Serialize};

/// One newline-delimited JSON request on the daemon control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum ControlRequest {
    StartDefinition {
        name: String,
        definition: Box<crate::config::ServerDef>,
    },
    Stop {
        name: String,
    },
    Restart {
        name: String,
    },
    Status {
        name: Option<String>,
    },
    Shutdown,
}

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
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_use_the_lowercase_op_wire_tag() -> serde_json::Result<()> {
        for (request, expected) in [
            (
                ControlRequest::Stop {
                    name: "echo".into(),
                },
                serde_json::json!({"op":"stop","name":"echo"}),
            ),
            (
                ControlRequest::Restart {
                    name: "echo".into(),
                },
                serde_json::json!({"op":"restart","name":"echo"}),
            ),
            (
                ControlRequest::Status { name: None },
                serde_json::json!({"op":"status","name":null}),
            ),
            (
                ControlRequest::Status {
                    name: Some("echo".into()),
                },
                serde_json::json!({"op":"status","name":"echo"}),
            ),
            (
                ControlRequest::Shutdown,
                serde_json::json!({"op":"shutdown"}),
            ),
        ] {
            assert_eq!(serde_json::to_value(&request)?, expected);
            let decoded: ControlRequest = serde_json::from_value(expected.clone())?;
            assert_eq!(serde_json::to_value(&decoded)?, expected);
        }
        Ok(())
    }

    #[test]
    fn resolved_definition_round_trip_preserves_transport_options() -> serde_json::Result<()> {
        let definition = crate::config::ServerDef {
            configuration_entry: Some(crate::config::ConfigurationEntry {
                source: std::path::PathBuf::from("fixture.json"),
                name: "logical-fixture".into(),
                scope: None,
                credential_store: None,
            }),
            url: "https://example.invalid/mcp".to_string(),
            headers: std::collections::BTreeMap::from([(
                "X-Fixture".to_string(),
                "configured".to_string(),
            )]),
            cwd: Some(std::path::PathBuf::from("fixture-directory")),
            timeout_ms: Some(120_000),
            ..Default::default()
        };
        let request = ControlRequest::StartDefinition {
            name: "fixture-pool".to_string(),
            definition: Box::new(definition),
        };
        let encoded = serde_json::to_string(&request)?;
        let decoded: ControlRequest = serde_json::from_str(&encoded)?;
        let ControlRequest::StartDefinition { name, definition } = decoded else {
            return Err(serde_json::Error::io(std::io::Error::other(
                "unexpected control operation",
            )));
        };
        assert_eq!(name, "fixture-pool");
        assert_eq!(
            definition.configuration_entry,
            Some(crate::config::ConfigurationEntry {
                source: std::path::PathBuf::from("fixture.json"),
                name: "logical-fixture".into(),
                scope: None,
                credential_store: None,
            })
        );
        assert_eq!(
            definition.headers.get("X-Fixture").map(String::as_str),
            Some("configured")
        );
        assert_eq!(
            definition.cwd,
            Some(std::path::PathBuf::from("fixture-directory"))
        );
        assert_eq!(definition.timeout_ms, Some(120_000));
        Ok(())
    }
}
