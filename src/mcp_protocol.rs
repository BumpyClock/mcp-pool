use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) const MODERN_VERSION: &str = "2026-07-28";
pub(crate) const LEGACY_VERSION: &str = "2025-11-25";
pub(crate) const VERSION_META_KEY: &str = "io.modelcontextprotocol/protocolVersion";
pub(crate) const DISCOVERY_UNSUPPORTED: i64 = -32004;
const LEGACY_VERSIONS: &[&str] = &[LEGACY_VERSION, "2025-06-18", "2025-03-26", "2024-11-05"];

pub(crate) fn discovery_unsupported(code: i64, data: Option<&Value>) -> bool {
    code == -32601
        || (code == DISCOVERY_UNSUPPORTED
            && data
                .and_then(|data| data.get("httpStatus"))
                .and_then(Value::as_u64)
                .is_some_and(|status| matches!(status, 400 | 404 | 405 | 415)))
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
    parameters
        .get("_meta")
        .and_then(|metadata| metadata.get(VERSION_META_KEY))
        .and_then(Value::as_str)
        == Some(MODERN_VERSION)
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
