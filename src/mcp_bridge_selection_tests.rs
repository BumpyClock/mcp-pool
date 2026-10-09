use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::{Value, json};

use super::super::options::select_servers;
use crate::config::ServerDef;
use crate::server_config::{ConfiguredServer, ServerConfiguration};

#[test]
fn server_selection_uses_lifecycle_defaults_and_explicit_subset() -> Result<()> {
    let mut servers = BTreeMap::new();
    servers.insert(
        "alpha".to_owned(),
        configured_server("alpha", json!({"lifecycle":"keep-alive"})),
    );
    servers.insert(
        "beta".to_owned(),
        configured_server("beta", json!({"lifecycle":"ephemeral"})),
    );
    servers.insert(
        "playwright".to_owned(),
        configured_server("playwright", json!({})),
    );
    let configuration = ServerConfiguration {
        source: PathBuf::from("synthetic.json"),
        servers,
        warnings: Vec::new(),
    };
    let selected = select_servers(&configuration, None)?;
    assert_eq!(
        selected
            .iter()
            .map(|server| server.name.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "playwright"]
    );
    let selected = select_servers(&configuration, Some(&["alpha".to_owned()]))?;
    assert_eq!(selected.len(), 1);
    assert_eq!(
        selected.first().map(|server| server.name.as_str()),
        Some("alpha")
    );
    assert!(select_servers(&configuration, Some(&["beta".to_owned()])).is_err());
    Ok(())
}

fn configured_server(name: &str, raw: Value) -> ConfiguredServer {
    ConfiguredServer {
        name: name.to_owned(),
        definition: ServerDef {
            command: "fixture".to_owned(),
            ..ServerDef::default()
        },
        raw,
        source: PathBuf::from("synthetic.json"),
    }
}
