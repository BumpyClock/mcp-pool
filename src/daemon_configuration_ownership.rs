use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::config::ConfigurationEntry;
use crate::control::ControlResponse;
use crate::types::{McpServerStatus, PoolStatusResponse};

pub(crate) enum Selection<'a> {
    Native {
        source: &'a Path,
        name: &'a str,
        scope: Option<&'a Path>,
    },
    Configuration {
        source: &'a Path,
        name: &'a str,
    },
    Credentials(&'a ConfigurationEntry),
}

impl Selection<'_> {
    pub(crate) fn matches(&self, server: &McpServerStatus) -> Result<bool> {
        let Some(associated) = &server.configuration_entry else {
            return Ok(false);
        };
        match self {
            Self::Native {
                source,
                name,
                scope,
            } => {
                if !same_configuration(associated, source, name) {
                    return Ok(false);
                }
                if associated.scope.is_none() {
                    bail!(
                        "Daemon lacks credential-scope ownership metadata; restart the daemon after upgrading"
                    );
                }
                Ok(associated.scope.as_deref() == *scope)
            }
            Self::Configuration { source, name } => {
                Ok(same_configuration(associated, source, name))
            }
            Self::Credentials(entry) => {
                let same_configuration = same_configuration(associated, &entry.source, &entry.name);
                if same_configuration && entry.scope.is_some() && associated.scope.is_none() {
                    bail!(
                        "Pool mutation blocked: daemon lacks credential-scope ownership metadata; restart the daemon after upgrading"
                    );
                }
                // Vault URL bindings are per logical name, even across config sources.
                Ok(associated.name == entry.name
                    && ((same_configuration
                        && entry
                            .scope
                            .as_ref()
                            .is_none_or(|scope| associated.scope.as_ref() == Some(scope)))
                        || entry.credential_store.as_ref().is_some_and(|store| {
                            associated.credential_store.as_ref() == Some(store)
                        })))
            }
        }
    }
}

fn same_configuration(entry: &ConfigurationEntry, source: &Path, name: &str) -> bool {
    entry.source == source && entry.name == name
}

pub(crate) fn status(response: &ControlResponse) -> Result<PoolStatusResponse> {
    if !response.ok {
        bail!(
            "{}",
            response
                .error
                .as_deref()
                .unwrap_or("Daemon operation failed")
        );
    }
    let data = response
        .data
        .as_ref()
        .context("Daemon status omitted servers; pool mutation was not attempted")?;
    serde_json::from_value(data.clone())
        .context("Invalid daemon status or configuration-entry ownership metadata")
}

pub(crate) fn readable(
    mut response: ControlResponse,
    selection: Option<Selection<'_>>,
) -> Result<ControlResponse> {
    if !response.ok || (response.data.is_none() && selection.is_none()) {
        return Ok(response);
    }
    let status = status(&response)?;
    let servers = response
        .data
        .as_mut()
        .and_then(|data| data.get_mut("servers"))
        .and_then(serde_json::Value::as_array_mut)
        .context("Daemon status omitted servers")?;
    let mut retained = Vec::new();
    // Keep the wire objects so status display never discards additional daemon metadata.
    for (server, mut wire) in status.servers.iter().zip(std::mem::take(servers)) {
        if selection
            .as_ref()
            .map(|selection| selection.matches(server))
            .transpose()?
            == Some(false)
        {
            continue;
        }
        if let Some(entry) = &server.configuration_entry {
            let object = wire
                .as_object_mut()
                .context("Invalid daemon status server")?;
            object.insert("name".into(), json!(entry.name));
            object.insert("pool_name".into(), json!(server.name));
        }
        retained.push(wire);
    }
    *servers = retained;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ServerReadiness, ServerStatus};

    fn entry() -> ConfigurationEntry {
        ConfigurationEntry {
            source: "fixture.json".into(),
            name: "fixture".into(),
            scope: Some("project".into()),
            credential_store: Some("project-credentials.json".into()),
        }
    }

    fn server(entry: Option<ConfigurationEntry>) -> McpServerStatus {
        McpServerStatus {
            name: "fixture-pool".into(),
            status: ServerStatus::Running,
            socket_path: "fixture-socket".into(),
            uptime_seconds: Some(12),
            connection_count: 2,
            owned: true,
            transport: "stdio".into(),
            readiness: ServerReadiness::default(),
            configuration_entry: entry,
        }
    }

    #[test]
    fn native_control_selects_exact_source_name_and_scope() -> Result<()> {
        let target = entry();
        let selection = Selection::Native {
            source: &target.source,
            name: &target.name,
            scope: target.scope.as_deref(),
        };
        assert!(selection.matches(&server(Some(target.clone())))?);
        let mut other_scope = target.clone();
        other_scope.scope = Some("other-project".into());
        assert!(!selection.matches(&server(Some(other_scope)))?);
        let mut other_source = target.clone();
        other_source.source = "other.json".into();
        assert!(!selection.matches(&server(Some(other_source)))?);
        let mut other_name = target.clone();
        other_name.name = "other".into();
        assert!(!selection.matches(&server(Some(other_name)))?);
        assert!(!selection.matches(&server(None))?);
        let mut missing_scope = target.clone();
        missing_scope.scope = None;
        assert!(
            selection
                .matches(&server(Some(missing_scope)))
                .err()
                .context("expected old daemon rejection")?
                .to_string()
                .contains("Daemon lacks credential-scope ownership metadata")
        );
        Ok(())
    }

    #[test]
    fn configuration_and_credentials_use_different_valid_ownership_policies() -> Result<()> {
        let target = entry();
        let configuration = Selection::Configuration {
            source: &target.source,
            name: &target.name,
        };
        let credentials = Selection::Credentials(&target);
        let mut other_scope = target.clone();
        other_scope.scope = Some("other-project".into());
        other_scope.credential_store = Some("other-project-credentials.json".into());
        let other_scope = server(Some(other_scope));
        assert!(configuration.matches(&other_scope)?);
        assert!(!credentials.matches(&other_scope)?);

        let mut inherited_store = target.clone();
        inherited_store.source = "other.json".into();
        inherited_store.scope = Some("other-project".into());
        let inherited_store = server(Some(inherited_store));
        assert!(!configuration.matches(&inherited_store)?);
        assert!(credentials.matches(&inherited_store)?);
        let mut other_name = target.clone();
        other_name.name = "another-name-in-the-same-store".into();
        assert!(!credentials.matches(&server(Some(other_name)))?);
        Ok(())
    }

    #[test]
    fn readable_status_preserves_counts_envelope_and_all_wire_metadata() -> Result<()> {
        let target = entry();
        let first = server(Some(target.clone()));
        let mut other_scope = target.clone();
        other_scope.scope = Some("other-project".into());
        let mut data = serde_json::to_value(PoolStatusResponse {
            server_count: 2,
            servers: vec![first, server(Some(other_scope))],
        })?;
        let original = data.clone();
        data.as_object_mut()
            .context("expected object")?
            .insert("future_daemon_metadata".into(), json!({"retained":true}));
        let first = data
            .pointer_mut("/servers/0")
            .and_then(serde_json::Value::as_object_mut)
            .context("expected first server")?;
        first.insert("future_server_metadata".into(), json!({"retained":true}));
        first
            .get_mut("configuration_entry")
            .and_then(serde_json::Value::as_object_mut)
            .context("expected ownership")?
            .insert("future_ownership_metadata".into(), json!(true));
        let response = readable(
            ControlResponse {
                ok: true,
                error: Some("retained envelope metadata".into()),
                data: Some(data),
            },
            Some(Selection::Native {
                source: &target.source,
                name: &target.name,
                scope: target.scope.as_deref(),
            }),
        )?;
        assert_eq!(
            response.error.as_deref(),
            Some("retained envelope metadata")
        );
        let data = response.data.context("expected readable data")?;
        let servers = data
            .get("servers")
            .and_then(serde_json::Value::as_array)
            .context("expected servers")?;
        assert_eq!(servers.len(), 1);
        assert_eq!(data.get("server_count"), Some(&json!(2)));
        assert_eq!(data.pointer("/servers/0/name"), Some(&json!("fixture")));
        assert_eq!(
            data.pointer("/servers/0/pool_name"),
            Some(&json!("fixture-pool"))
        );
        for field in [
            "status",
            "socket_path",
            "uptime_seconds",
            "connection_count",
            "owned",
            "transport",
            "readiness",
        ] {
            assert_eq!(
                data.pointer(&format!("/servers/0/{field}")),
                original.pointer(&format!("/servers/0/{field}"))
            );
        }
        assert_eq!(
            data.pointer("/servers/0/configuration_entry/source"),
            Some(&json!(target.source))
        );
        assert_eq!(
            data.pointer("/servers/0/configuration_entry/scope"),
            Some(&json!(target.scope))
        );
        assert_eq!(
            data.pointer("/servers/0/configuration_entry/credential_store"),
            Some(&json!(target.credential_store))
        );
        assert_eq!(
            data.pointer("/future_daemon_metadata/retained"),
            Some(&json!(true))
        );
        assert_eq!(
            data.pointer("/servers/0/future_server_metadata/retained"),
            Some(&json!(true))
        );
        assert_eq!(
            data.pointer("/servers/0/configuration_entry/future_ownership_metadata"),
            Some(&json!(true))
        );
        Ok(())
    }

    #[test]
    fn control_errors_and_shutdown_acknowledgements_are_not_status_snapshots() -> Result<()> {
        let response = readable(ControlResponse::err("retirement failed"), None)?;
        assert!(!response.ok);
        assert_eq!(response.error.as_deref(), Some("retirement failed"));
        assert!(readable(ControlResponse::ok(), None)?.data.is_none());
        assert!(
            readable(
                ControlResponse::ok(),
                Some(Selection::Configuration {
                    source: Path::new("fixture.json"),
                    name: "fixture",
                }),
            )
            .is_err()
        );
        assert!(status(&ControlResponse::data(json!({"servers":[]}))).is_err());
        Ok(())
    }
}
