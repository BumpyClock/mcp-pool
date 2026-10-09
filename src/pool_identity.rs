use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::config::ServerDef;
use crate::server_config::ConfiguredServer;

pub(super) fn pool_name(server: &ConfiguredServer, definition: &ServerDef) -> Result<String> {
    let mut definition = serde_json::to_value(definition)?;
    if let Some(object) = definition.as_object_mut() {
        object.remove("configuration_entry");
        if let Some(environment) = object.get_mut("env").and_then(Value::as_object_mut) {
            let configured = server.raw.get("env").and_then(Value::as_object);
            environment.retain(|name, _| {
                configured.is_some_and(|keys| {
                    keys.keys().any(|key| {
                        #[cfg(windows)]
                        {
                            key.eq_ignore_ascii_case(name)
                        }
                        #[cfg(not(windows))]
                        {
                            key == name
                        }
                    })
                })
            });
            if environment.is_empty() {
                object.remove("env");
            }
        }
    }
    let content =
        canonical(json!({"source":server.source,"name":server.name,"definition":definition}));
    let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&content)?));
    let digest = digest.get(..32).context("Invalid identity digest")?;
    let label: String = server
        .name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .take(16)
        .collect();
    #[cfg(unix)]
    let budget = {
        use std::os::unix::ffi::OsStrExt;
        let overhead = crate::config::server_socket_path("x")
            .as_os_str()
            .as_bytes()
            .len()
            .saturating_sub(1);
        103usize.saturating_sub(overhead)
    };
    #[cfg(windows)]
    let budget = {
        use std::os::windows::ffi::OsStrExt;
        let overhead = crate::config::server_socket_path("x")
            .as_os_str()
            .encode_wide()
            .count()
            .saturating_sub(1);
        240usize.saturating_sub(overhead)
    };
    bounded_name(&label, digest, budget)
}

fn bounded_name(label: &str, digest: &str, budget: usize) -> Result<String> {
    let minimum = "mcp-".len() + digest.len();
    if budget < minimum {
        bail!(
            "The MCP pool socket directory leaves insufficient space for a safe server identity. Set MCP_POOL_HOME to a shorter directory."
        );
    }
    let label_budget = budget.saturating_sub(minimum + 1);
    let label = label
        .get(..label.len().min(label_budget))
        .context("Invalid pool label")?;
    if label.is_empty() {
        Ok(format!("mcp-{digest}"))
    } else {
        Ok(format!("mcp-{label}-{digest}"))
    }
}

fn canonical(value: Value) -> Value {
    match value {
        Value::Object(object) => {
            let sorted: std::collections::BTreeMap<_, _> = object
                .into_iter()
                .map(|(key, value)| (key, canonical(value)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonical).collect()),
        value => value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_fit_the_complete_socket_path_budget() -> Result<()> {
        let digest = "0123456789abcdef0123456789abcdef";
        assert_eq!(
            bounded_name("a-long-configured-label", digest, 45)?.len(),
            45
        );
        assert_eq!(bounded_name("label", digest, 36)?, format!("mcp-{digest}"));
        assert!(bounded_name("label", digest, 35).is_err());
        Ok(())
    }

    #[test]
    fn canonical_hashes_ignore_object_order_but_preserve_array_order() -> Result<()> {
        let first: Value = serde_json::from_str(r#"{"second":{"z":2,"a":1},"first":[1,2]}"#)?;
        let other: Value = serde_json::from_str(r#"{"first":[1,2],"second":{"a":1,"z":2}}"#)?;
        assert_eq!(
            serde_json::to_vec(&canonical(first))?,
            serde_json::to_vec(&canonical(other))?
        );
        assert_ne!(canonical(json!([1, 2])), canonical(json!([2, 1])));
        Ok(())
    }
}
