use serde_json::{Map, Value};

use super::ConfiguredServer;

pub(crate) struct OAuthSettings<'a> {
    pub auth: Option<&'a str>,
    pub token_cache_directory: Option<&'a str>,
    pub client_name: Option<&'a str>,
    pub client_id: Option<&'a str>,
    pub client_secret: Option<&'a str>,
    pub client_secret_env: Option<&'a str>,
    pub token_auth_method: Option<&'a str>,
    pub redirect_url: Option<&'a str>,
    pub client_metadata_url: Option<&'a str>,
    pub scope: Option<&'a str>,
    pub helper_arguments: Option<&'a [Value]>,
}

pub(crate) struct ConfiguredEnvironment<'a> {
    entries: Option<&'a Map<String, Value>>,
}

impl ConfiguredEnvironment<'_> {
    // Error redaction historically matches exact names; pool identity follows OS semantics.
    pub(crate) fn contains_exact(&self, name: &str) -> bool {
        self.entries
            .is_some_and(|entries| entries.contains_key(name))
    }

    pub(crate) fn contains(&self, name: &str) -> bool {
        self.entries
            .is_some_and(|entries| entries.keys().any(|key| environment_names_match(key, name)))
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries
            .into_iter()
            .flat_map(|entries| entries.keys().map(String::as_str))
    }
}

impl ConfiguredServer {
    pub(crate) fn oauth_settings(&self) -> OAuthSettings<'_> {
        let raw = &self.raw;
        OAuthSettings {
            auth: nonempty_string(raw, "auth"),
            token_cache_directory: string_alias(raw, "tokenCacheDir", "token_cache_dir"),
            client_name: string_alias(raw, "clientName", "client_name"),
            client_id: string_alias(raw, "oauthClientId", "oauth_client_id"),
            client_secret: string_alias(raw, "oauthClientSecret", "oauth_client_secret"),
            client_secret_env: string_alias(raw, "oauthClientSecretEnv", "oauth_client_secret_env"),
            token_auth_method: string_alias(
                raw,
                "oauthTokenEndpointAuthMethod",
                "oauth_token_endpoint_auth_method",
            ),
            redirect_url: string_alias(raw, "oauthRedirectUrl", "oauth_redirect_url"),
            client_metadata_url: string_alias(
                raw,
                "oauthClientMetadataUrl",
                "oauth_client_metadata_url",
            ),
            scope: string_alias(raw, "oauthRequestedScope", "oauth_requested_scope")
                .or_else(|| string_alias(raw, "oauthScope", "oauth_scope")),
            helper_arguments: raw
                .get("oauthCommand")
                .or_else(|| raw.get("oauth_command"))
                .and_then(|value| value.get("args"))
                .and_then(Value::as_array)
                .map(Vec::as_slice),
        }
    }

    pub(crate) fn configured_environment(&self) -> ConfiguredEnvironment<'_> {
        ConfiguredEnvironment {
            entries: self.raw.get("env").and_then(Value::as_object),
        }
    }

    pub(crate) fn configured_lifecycle(&self) -> Option<bool> {
        let lifecycle = self.raw.get("lifecycle")?;
        let mode = match lifecycle {
            Value::String(value) => Some(value.as_str()),
            Value::Object(_) => lifecycle.get("mode").and_then(Value::as_str),
            _ => None,
        };
        match mode {
            Some("keep-alive") => Some(true),
            Some("ephemeral") => Some(false),
            _ => None,
        }
    }

    pub(crate) fn tool_filter(&self) -> anyhow::Result<crate::tool_filter::ToolFilter> {
        use anyhow::Context;

        crate::tool_filter::ToolFilter::from_raw(&self.raw)
            .with_context(|| format!("Server '{}' has invalid tool filters", self.name))
    }
}

pub(super) fn environment_names_match(configured: &str, name: &str) -> bool {
    if cfg!(windows) {
        configured.eq_ignore_ascii_case(name)
    } else {
        configured == name
    }
}

fn nonempty_string<'a>(raw: &'a Value, key: &str) -> Option<&'a str> {
    raw.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn string_alias<'a>(raw: &'a Value, camel: &str, snake: &str) -> Option<&'a str> {
    nonempty_string(raw, camel).or_else(|| nonempty_string(raw, snake))
}
