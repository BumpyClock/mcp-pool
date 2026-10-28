use super::*;
use crate::config::ConfigurationEntry;
use crate::types::{McpServerStatus, PoolStatusResponse, ServerReadiness, ServerStatus};
use std::sync::{Arc, Mutex};

fn entry() -> ConfigurationEntry {
    ConfigurationEntry {
        source: "synthetic.json".into(),
        name: "fixture".to_owned(),
        scope: None,
        credential_store: None,
    }
}

fn server(
    name: &str,
    owned: bool,
    configuration_entry: Option<ConfigurationEntry>,
) -> McpServerStatus {
    McpServerStatus {
        name: name.into(),
        status: ServerStatus::Running,
        socket_path: "synthetic-socket".into(),
        uptime_seconds: Some(12),
        connection_count: 2,
        owned,
        transport: "stdio".into(),
        readiness: ServerReadiness::default(),
        configuration_entry,
    }
}

fn status(servers: Vec<McpServerStatus>) -> PoolStatusResponse {
    PoolStatusResponse {
        server_count: servers.len(),
        servers,
    }
}

#[tokio::test]
async fn credential_mutation_retires_only_related_owned_environment_views() -> Result<()> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = events.clone();
    let mut other_source = entry();
    other_source.source = "other.json".into();
    let mut other_name = entry();
    other_name.name = "other".into();
    retire_matching(
        &status(vec![
            server("old-env", true, Some(entry())),
            server("new-env", true, Some(entry())),
            server("other-source", true, Some(other_source)),
            server("other-name", false, Some(other_name)),
            server("native", true, None),
        ]),
        &Selection::Credentials(&entry()),
        move |request| {
            let recorded = recorded.clone();
            async move {
                match request {
                    ControlRequest::Stop { name } => {
                        recorded
                            .lock()
                            .map_err(|_| anyhow::anyhow!("test lock poisoned"))?
                            .push(format!("stop:{name}"));
                        Ok(ControlResponse::ok())
                    }
                    _ => bail!("Unexpected control request"),
                }
            }
        },
    )
    .await?;
    assert_eq!(
        *events
            .lock()
            .map_err(|_| anyhow::anyhow!("test lock poisoned"))?,
        vec!["stop:old-env", "stop:new-env"]
    );
    Ok(())
}

#[tokio::test]
async fn unverified_retirement_prohibits_credential_mutation() -> Result<()> {
    let result = retire_matching(
        &status(vec![server("matching", true, Some(entry()))]),
        &Selection::Credentials(&entry()),
        |_| async { Ok(ControlResponse::err("retirement unverified")) },
    )
    .await;
    assert!(
        result
            .err()
            .context("expected error")?
            .to_string()
            .contains("Pool mutation blocked")
    );
    assert!(ownership::status(&ControlResponse::data(json!({}))).is_err());
    assert!(
        ownership::status(&ControlResponse::data(json!({"server_count":1,"servers":[{
            "name":"incomplete","owned":true,"configuration_entry":entry()
        }]})))
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn borrowed_generation_cannot_authorize_credential_or_config_mutation() -> Result<()> {
    let target = entry();
    for selection in [
        Selection::Credentials(&target),
        Selection::Configuration {
            source: &target.source,
            name: &target.name,
        },
    ] {
        let result = retire_matching(
            &status(vec![server("matching", false, Some(entry()))]),
            &selection,
            |_| async {
                Err(anyhow::anyhow!(
                    "Unexpected stop request for borrowed generation"
                ))
            },
        )
        .await;
        assert!(
            result
                .err()
                .context("expected ownership rejection")?
                .to_string()
                .contains("not a verified owned generation")
        );
    }
    Ok(())
}

#[tokio::test]
async fn home_auth_mutation_retires_every_consumer_of_that_store_but_not_project_accounts()
-> Result<()> {
    let target = ConfigurationEntry {
        source: "home/config.json".into(),
        name: "fixture".into(),
        scope: Some("home".into()),
        credential_store: Some("home/credentials.json".into()),
    };
    let mut inherited = target.clone();
    inherited.source = "project/config.json".into();
    inherited.scope = Some("project".into());
    let mut independent = inherited.clone();
    independent.credential_store = Some("project/credentials.json".into());
    let stopped = Arc::new(Mutex::new(Vec::new()));
    let recorded = stopped.clone();
    retire_matching(
        &status(vec![
            server("home-view", true, Some(target.clone())),
            server("inherited-view", true, Some(inherited)),
            server("project-account", true, Some(independent)),
        ]),
        &Selection::Credentials(&target),
        move |request| {
            let recorded = recorded.clone();
            async move {
                if let ControlRequest::Stop { name } = request {
                    recorded
                        .lock()
                        .map_err(|_| anyhow::anyhow!("test lock poisoned"))?
                        .push(name);
                } else {
                    bail!("Unexpected control operation");
                }
                Ok(ControlResponse::ok())
            }
        },
    )
    .await?;
    assert_eq!(
        *stopped
            .lock()
            .map_err(|_| anyhow::anyhow!("test lock poisoned"))?,
        ["home-view", "inherited-view"]
    );
    Ok(())
}

#[tokio::test]
async fn configuration_mutation_retires_all_scopes_but_not_another_source_or_name() -> Result<()> {
    let mut first = entry();
    first.scope = Some("first-project".into());
    first.credential_store = Some("shared-credentials.json".into());
    let mut second = first.clone();
    second.scope = Some("second-project".into());
    let mut other_source = first.clone();
    other_source.source = "other.json".into();
    let mut other_name = first.clone();
    other_name.name = "other".into();
    let mut stopped = Vec::new();
    retire_matching(
        &status(vec![
            server("first", true, Some(first)),
            server("second", true, Some(second)),
            server("unscoped", true, Some(entry())),
            server("other-source", true, Some(other_source)),
            server("other-name", true, Some(other_name)),
        ]),
        &Selection::Configuration {
            source: std::path::Path::new("synthetic.json"),
            name: "fixture",
        },
        |request| {
            if let ControlRequest::Stop { name } = request {
                stopped.push(name);
            }
            async { Ok(ControlResponse::ok()) }
        },
    )
    .await?;
    assert_eq!(stopped, ["first", "second", "unscoped"]);
    Ok(())
}

#[tokio::test]
async fn missing_scope_blocks_scoped_credential_retirement_without_stopping_the_view() -> Result<()>
{
    let mut target = entry();
    target.scope = Some("project".into());
    let result = retire_matching(
        &status(vec![server("old-daemon-view", true, Some(entry()))]),
        &Selection::Credentials(&target),
        |_| async { bail!("must reject missing scope before sending Stop") },
    )
    .await;
    assert!(
        result
            .err()
            .context("expected missing-scope rejection")?
            .to_string()
            .contains("daemon lacks credential-scope ownership metadata")
    );
    Ok(())
}

#[tokio::test]
async fn absent_daemon_allows_mutation_without_any_stop_or_launch_request() -> Result<()> {
    let mut requests = Vec::new();
    retire_selection_with(&Selection::Credentials(&entry()), |request| {
        requests.push(request);
        async { Ok(None) }
    })
    .await?;
    assert_eq!(requests.len(), 1);
    assert!(matches!(
        requests.first(),
        Some(ControlRequest::Status { name: None })
    ));
    Ok(())
}

#[tokio::test]
async fn daemon_disappearance_after_status_does_not_authorize_mutation_or_replay_stop() -> Result<()>
{
    let response = ControlResponse::data(serde_json::to_value(status(vec![
        server("first", true, Some(entry())),
        server("second", true, Some(entry())),
    ]))?);
    let mut requests = Vec::new();
    let result = retire_selection_with(&Selection::Credentials(&entry()), |request| {
        let response = matches!(&request, ControlRequest::Status { .. }).then(|| response.clone());
        requests.push(request);
        async move { Ok(response) }
    })
    .await;
    assert_eq!(requests.len(), 2);
    assert!(matches!(
        requests.get(1),
        Some(ControlRequest::Stop { name }) if name == "first"
    ));
    assert!(
        result
            .err()
            .context("expected failed retirement confirmation")?
            .to_string()
            .contains("daemon disappeared before confirming pool retirement")
    );
    Ok(())
}
