use std::collections::BTreeMap;

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Map, Value};

pub(crate) const URL_FIELDS: &[&str] = &["baseUrl", "base_url", "url", "serverUrl", "server_url"];

pub(crate) fn is_remote_entry(entry: &Map<String, Value>) -> bool {
    URL_FIELDS.iter().any(|field| entry.contains_key(*field))
}

pub(crate) fn is_server_entry(entry: &Map<String, Value>) -> bool {
    is_remote_entry(entry) || entry.contains_key("command") || entry.contains_key("executable")
}

pub(super) fn selected_url(raw: &Value) -> Option<&str> {
    URL_FIELDS
        .iter()
        .find_map(|field| raw.get(*field).and_then(Value::as_str))
}

#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum Command {
    Text(String),
    Tokens(Vec<String>),
}

#[derive(Clone, Copy)]
pub(super) enum EntryPolicy {
    New,
    Preserve,
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct Entry {
    #[serde(skip)]
    url: Option<String>,
    pub command: Option<Command>,
    pub executable: Option<String>,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub transport: Option<String>,
    pub description: Option<String>,
    #[serde(rename = "timeoutMs", alias = "timeout_ms")]
    pub timeout_ms: Option<u64>,
    #[serde(rename = "protocolVersion", alias = "protocol_version")]
    pub protocol_version: crate::mcp_protocol::ProtocolPreference,
    #[serde(rename = "bearerToken", alias = "bearer_token")]
    pub bearer_token: Option<String>,
    #[serde(rename = "bearerTokenEnv", alias = "bearer_token_env")]
    pub bearer_token_env: Option<String>,
}

impl Entry {
    pub(super) fn decode(raw: &Value) -> Result<Self> {
        let mut entry: Self = serde_json::from_value(raw.clone())
            .map_err(|_| anyhow!("Invalid server field types"))?;
        for field in URL_FIELDS {
            if raw
                .get(*field)
                .is_some_and(|value| !value.is_null() && !value.is_string())
            {
                bail!("Invalid URL field type");
            }
        }
        entry.url = selected_url(raw).map(str::to_owned);
        Ok(entry)
    }

    pub(super) fn remote_url(&self) -> Option<&str> {
        self.url.as_deref().filter(|url| !url.is_empty())
    }

    pub(super) fn transport(&self, transport: &str) -> Result<&'static str> {
        match (self.remote_url().is_some(), transport) {
            (true, "" | "http" | "streamable-http") => Ok("http"),
            (true, "sse") => Ok("sse"),
            (false, "" | "stdio") => Ok("stdio"),
            (true, _) => bail!("field 'transport' must be http or sse for a URL"),
            (false, _) => bail!("field 'transport' must be stdio for a command"),
        }
    }

    pub(super) fn validate(&self, policy: EntryPolicy) -> Result<()> {
        let command = self.command.is_some() || self.executable.is_some();
        if matches!(policy, EntryPolicy::New) && self.url.is_some() == command {
            bail!("needs exactly one URL or command");
        }
        if let Some(url) = self.remote_url() {
            if !url.contains('$') {
                validate_url(url)?;
            }
        } else {
            let executable = match &self.command {
                Some(Command::Text(command)) => Some(command.as_str()),
                Some(Command::Tokens(tokens)) => tokens.first().map(String::as_str),
                None => self.executable.as_deref(),
            };
            if executable.is_none_or(|value| value.trim().is_empty()) {
                bail!("needs a nonempty executable");
            }
        }
        let transport = self.transport.as_deref().unwrap_or_default();
        if !matches!(policy, EntryPolicy::Preserve) || !transport.contains('$') {
            self.transport(transport)?;
        }
        Ok(())
    }
}

pub(super) fn validate_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| anyhow!("field 'url' must be an absolute HTTP/SSE URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        bail!("field 'url' must be an absolute HTTP/SSE URL");
    }
    Ok(())
}
