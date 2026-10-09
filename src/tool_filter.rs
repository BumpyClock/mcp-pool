use std::collections::BTreeSet;

use anyhow::{Result, bail};
use serde_json::Value;

#[derive(Clone, Default)]
pub(crate) struct ToolFilter {
    allowed: Option<BTreeSet<String>>,
    blocked: Option<BTreeSet<String>>,
}

impl ToolFilter {
    pub(crate) fn from_raw(raw: &Value) -> Result<Self> {
        let allowed = parse_names(raw, "allowedTools", "allowed_tools")?;
        let blocked = parse_names(raw, "blockedTools", "blocked_tools")?;
        if allowed.is_some() && blocked.is_some() {
            bail!("A server cannot configure both allowedTools and blockedTools.");
        }
        Ok(Self { allowed, blocked })
    }

    pub(crate) fn permits(&self, name: &str) -> bool {
        match (&self.allowed, &self.blocked) {
            (Some(allowed), _) => allowed.contains(name),
            (_, Some(blocked)) => !blocked.contains(name),
            (None, None) => true,
        }
    }
}

fn parse_names(
    raw: &Value,
    camel_case: &str,
    snake_case: &str,
) -> Result<Option<BTreeSet<String>>> {
    let Some(value) = raw.get(camel_case).or_else(|| raw.get(snake_case)) else {
        return Ok(None);
    };
    let Some(values) = value.as_array() else {
        bail!("Server tool filters must be arrays of tool names.");
    };
    let mut names = BTreeSet::new();
    for value in values {
        let Some(name) = value.as_str() else {
            bail!("Server tool filters must be arrays of tool names.");
        };
        names.insert(name.to_owned());
    }
    Ok(Some(names))
}
