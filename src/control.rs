use serde::{Deserialize, Serialize};

/// One newline-delimited JSON request on the daemon control socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum ControlRequest {
    Start {
        name: String,
    },
    StartDefinition {
        name: String,
        definition: Box<crate::config::ServerDef>,
    },
    StartAll,
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
    fn request_is_tagged_with_op() {
        let serialized = serde_json::to_string(&ControlRequest::Start {
            name: "echo".into(),
        })
        .unwrap();
        assert!(serialized.contains("\"op\":\"start\""), "{serialized}");
        assert!(serialized.contains("\"name\":\"echo\""), "{serialized}");
        let back: ControlRequest = serde_json::from_str(&serialized).unwrap();
        assert!(matches!(back, ControlRequest::Start { name } if name == "echo"));
    }

    #[test]
    fn all_request_variants_round_trip() {
        let serializations = [
            serde_json::to_string(&ControlRequest::Stop { name: "a".into() }).unwrap(),
            serde_json::to_string(&ControlRequest::StartAll).unwrap(),
            serde_json::to_string(&ControlRequest::Restart { name: "a".into() }).unwrap(),
            serde_json::to_string(&ControlRequest::Status { name: None }).unwrap(),
            serde_json::to_string(&ControlRequest::Status {
                name: Some("a".into()),
            })
            .unwrap(),
            serde_json::to_string(&ControlRequest::Shutdown).unwrap(),
        ];
        for serialized in serializations {
            let _: ControlRequest = serde_json::from_str(&serialized).unwrap();
        }
    }

    #[test]
    fn response_helpers() {
        assert!(ControlResponse::ok().ok);
        assert!(!ControlResponse::err("boom").ok);
        assert_eq!(ControlResponse::err("boom").error.as_deref(), Some("boom"));
        assert!(
            ControlResponse::data(serde_json::json!({ "x": 1 }))
                .data
                .is_some()
        );
    }

    #[test]
    fn resolved_definition_round_trip_preserves_transport_options() -> serde_json::Result<()> {
        let definition = crate::config::ServerDef {
            configuration_entry: Some(crate::config::ConfigurationEntry {
                source: std::path::PathBuf::from("fixture.json"),
                name: "logical-fixture".into(),
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
