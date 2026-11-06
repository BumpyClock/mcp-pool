use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const MODERN_VERSION: &str = "2026-07-28";
pub(crate) const LEGACY_VERSION: &str = "2025-11-25";
pub(crate) const VERSION_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";
pub(crate) const SUBSCRIPTION_ID_META_KEY: &str = "io.modelcontextprotocol/subscriptionId";
pub(crate) const SERVER_INFO_META_KEY: &str = "io.modelcontextprotocol/serverInfo";
pub(crate) const PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
pub(crate) const SESSION_ID_HEADER: &str = "mcp-session-id";
pub(crate) const METHOD_HEADER: &str = "mcp-method";
pub(crate) const NAME_HEADER: &str = "mcp-name";
pub(crate) const DISCOVERY_UNSUPPORTED: i64 = -32004;
pub(crate) const LEGACY_VERSIONS: &[&str] =
    &[LEGACY_VERSION, "2025-06-18", "2025-03-26", "2024-11-05"];

pub(crate) fn discovery_unsupported(code: i64, data: Option<&Value>) -> bool {
    code == -32601
        || (code == DISCOVERY_UNSUPPORTED
            && data
                .and_then(|data| data.get("httpStatus"))
                .and_then(Value::as_u64)
                .is_some_and(discovery_http_rejection))
}

pub(crate) fn discovery_http_rejection(status: u64) -> bool {
    matches!(status, 400 | 404 | 405 | 415)
}

pub(crate) fn discovery_rejection_data(status: u16) -> Value {
    json!({"httpStatus":status,"mcpCliFailure":TransportFailure {
        kind: FailureKind::Http, status_code: Some(status)
    }})
}

pub(crate) fn legacy_http_rejection(status: u16) -> bool {
    matches!(status, 404 | 405)
}

pub(crate) fn supports_version(version: &str) -> bool {
    version == MODERN_VERSION || LEGACY_VERSIONS.contains(&version)
}

pub(crate) fn negotiate_legacy_version(requested: &str) -> &str {
    if LEGACY_VERSIONS.contains(&requested) {
        requested
    } else {
        LEGACY_VERSION
    }
}

pub(crate) fn protocol_metadata(parameters: &Value) -> Option<&str> {
    parameters.get("_meta")?.get(VERSION_META_KEY)?.as_str()
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub(crate) enum FailureKind {
    Auth,
    Offline,
    Timeout,
    Http,
    Error,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TransportFailure {
    pub(crate) kind: FailureKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) status_code: Option<u16>,
}

impl TransportFailure {
    pub(crate) fn data(self) -> Value {
        json!({"mcpCliFailure":self})
    }

    pub(crate) fn from_data(data: &Value) -> Option<Self> {
        serde_json::from_value(data.get("mcpCliFailure")?.clone()).ok()
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub enum ProtocolPreference {
    #[default]
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "legacy")]
    Legacy,
    #[serde(rename = "2026-07-28")]
    Modern,
}

impl ProtocolPreference {
    pub(crate) fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConnectionInfo {
    pub(crate) protocol_version: String,
    pub(crate) era: Era,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) instructions: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Era {
    #[default]
    Legacy,
    Modern,
}

pub(crate) fn stamp(parameters: &mut Value) -> Result<()> {
    let parameters = parameters
        .as_object_mut()
        .context("Modern MCP parameters must be an object")?;
    let metadata = parameters.entry("_meta").or_insert_with(|| json!({}));
    let metadata = metadata
        .as_object_mut()
        .context("Modern MCP _meta must be an object")?;
    metadata.insert(VERSION_META_KEY.into(), json!(MODERN_VERSION));
    Ok(())
}

pub(crate) fn is_modern(parameters: &Value) -> bool {
    protocol_metadata(parameters) == Some(MODERN_VERSION)
}

pub(crate) fn discovery_offers_modern(result: &Value) -> Result<bool> {
    let versions = result
        .get("supportedVersions")
        .and_then(Value::as_array)
        .filter(|versions| !versions.is_empty())
        .context("MCP server/discover omitted a nonempty supportedVersions array")?;
    if versions
        .iter()
        .any(|version| version.as_str().is_none_or(str::is_empty))
    {
        bail!("MCP server/discover returned invalid supportedVersions");
    }
    Ok(versions.iter().any(|version| version == MODERN_VERSION))
}

pub(crate) fn connection_info(result: &Value, era: Era) -> Result<ConnectionInfo> {
    if !result.get("capabilities").is_some_and(Value::is_object) {
        bail!("MCP handshake omitted capabilities");
    }
    let protocol_version = if era == Era::Modern {
        if !discovery_offers_modern(result)? {
            bail!("MCP server does not support pinned protocol version {MODERN_VERSION}");
        }
        MODERN_VERSION
    } else {
        let version = result
            .get("protocolVersion")
            .and_then(Value::as_str)
            .context("MCP initialize result omitted protocolVersion")?;
        if !LEGACY_VERSIONS.contains(&version) {
            bail!("unsupported MCP protocol version: {version}");
        }
        version
    };
    let instructions = result
        .get("instructions")
        .map(|instructions| {
            instructions
                .as_str()
                .map(str::to_owned)
                .context("MCP handshake instructions must be a string")
        })
        .transpose()?;
    Ok(ConnectionInfo {
        protocol_version: protocol_version.to_owned(),
        era,
        instructions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_failure_metadata_preserves_the_wire_contract() {
        for (kind, name) in [
            (FailureKind::Auth, "auth"),
            (FailureKind::Offline, "offline"),
            (FailureKind::Timeout, "timeout"),
            (FailureKind::Http, "http"),
            (FailureKind::Error, "error"),
        ] {
            let metadata = TransportFailure {
                kind,
                status_code: None,
            };
            assert_eq!(metadata.data(), json!({"mcpCliFailure":{"kind":name}}));
            assert_eq!(
                TransportFailure::from_data(&metadata.data()),
                Some(metadata)
            );
        }
        let metadata = TransportFailure {
            kind: FailureKind::Auth,
            status_code: Some(401),
        };
        assert_eq!(
            metadata.data(),
            json!({"mcpCliFailure":{"kind":"auth","statusCode":401}})
        );
        assert_eq!(
            TransportFailure::from_data(&metadata.data()),
            Some(metadata)
        );
        assert_eq!(
            TransportFailure::from_data(&json!({"httpStatus":401})),
            None
        );
        assert_eq!(
            TransportFailure::from_data(&json!({"mcpCliFailure":{"kind":"unknown"}})),
            None
        );
    }

    #[test]
    fn only_definitive_discovery_rejections_allow_fallback() {
        for status in [400, 404, 405, 415] {
            assert!(discovery_unsupported(
                DISCOVERY_UNSUPPORTED,
                Some(&json!({"httpStatus":status}))
            ));
        }
        for status in [401, 403, 408, 429, 500, 503] {
            assert!(!discovery_unsupported(
                DISCOVERY_UNSUPPORTED,
                Some(&json!({"httpStatus":status}))
            ));
        }
        assert!(discovery_unsupported(-32601, None));
        assert!(!discovery_unsupported(DISCOVERY_UNSUPPORTED, None));
    }

    #[test]
    fn modern_metadata_preserves_existing_request_metadata() -> Result<()> {
        let mut parameters = json!({"_meta":{"trace":"fixture"},"name":"echo"});
        stamp(&mut parameters)?;
        assert_eq!(
            parameters,
            json!({"_meta":{
                "trace":"fixture",
                "io.modelcontextprotocol/protocolVersion":"2026-07-28"
            },"name":"echo"})
        );
        assert!(is_modern(&parameters));
        assert!(stamp(&mut json!({"_meta":[]})).is_err());
        Ok(())
    }

    #[test]
    fn preferences_reject_unknown_or_misspelled_versions() -> Result<()> {
        for (encoded, expected) in [
            ("\"auto\"", ProtocolPreference::Auto),
            ("\"legacy\"", ProtocolPreference::Legacy),
            ("\"2026-07-28\"", ProtocolPreference::Modern),
        ] {
            assert_eq!(
                serde_json::from_str::<ProtocolPreference>(encoded)?,
                expected
            );
        }
        assert!(serde_json::from_str::<ProtocolPreference>("\"2026\"").is_err());
        Ok(())
    }

    #[test]
    fn discovery_distinguishes_legacy_offers_from_malformed_success() -> Result<()> {
        assert!(discovery_offers_modern(
            &json!({"supportedVersions":["2026-07-28"]})
        )?);
        assert!(!discovery_offers_modern(
            &json!({"supportedVersions":["2025-11-25"]})
        )?);
        for malformed in [
            json!({}),
            json!({"supportedVersions":[]}),
            json!({"supportedVersions":[null]}),
            json!({"supportedVersions":[""]}),
        ] {
            assert!(discovery_offers_modern(&malformed).is_err());
        }
        Ok(())
    }
}
