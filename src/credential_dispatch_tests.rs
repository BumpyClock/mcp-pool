use super::*;
use crate::config::ConfigurationEntry;
use std::sync::{Arc, Mutex};

fn entry() -> ConfigurationEntry {
    ConfigurationEntry {
        source: "synthetic.json".into(),
        name: "fixture".to_owned(),
    }
}

#[tokio::test]
async fn retirement_precedes_credentials_and_retires_all_related_environment_views() -> Result<()> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let recorded = events.clone();
    retire_matching(
        &json!({"servers":[
            {"name":"old-env","owned":true,"configuration_entry":entry()},
            {"name":"new-env","owned":true,"configuration_entry":entry()},
            {"name":"other-source","owned":true,"configuration_entry":{"source":"other.json","name":"fixture"}},
            {"name":"other-name","owned":false,"configuration_entry":{"source":"synthetic.json","name":"other"}},
            {"name":"native","owned":true}
        ]}),
        &entry(),
        move |request| {
            let recorded = recorded.clone();
            async move {
                match request {
                    ControlRequest::Stop { name } => {
                        recorded.lock().map_err(|_| anyhow::anyhow!("test lock poisoned"))?
                            .push(format!("stop:{name}"));
                        Ok(ControlResponse::ok())
                    }
                    _ => bail!("Unexpected control request"),
                }
            }
        },
    ).await?;
    events
        .lock()
        .map_err(|_| anyhow::anyhow!("test lock poisoned"))?
        .push("credentials:write".to_owned());
    assert_eq!(
        *events
            .lock()
            .map_err(|_| anyhow::anyhow!("test lock poisoned"))?,
        vec!["stop:old-env", "stop:new-env", "credentials:write"]
    );
    Ok(())
}

#[tokio::test]
async fn unverified_retirement_prohibits_credential_mutation() -> Result<()> {
    let result = retire_matching(
        &json!({"servers":[{"name":"matching","owned":true,"configuration_entry":entry()}]}),
        &entry(),
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
    assert!(
        retire_matching(&json!({}), &entry(), |_| async {
            Ok(ControlResponse::ok())
        })
        .await
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn borrowed_generation_cannot_authorize_credential_or_config_mutation() -> Result<()> {
    let status =
        json!({"servers":[{"name":"matching","owned":false,"configuration_entry":entry()}]});
    let result = retire_matching(&status, &entry(), |_| async {
        Err(anyhow::anyhow!(
            "Unexpected stop request for borrowed generation"
        ))
    })
    .await;
    assert!(
        result
            .err()
            .context("expected ownership rejection")?
            .to_string()
            .contains("not a verified owned generation")
    );
    Ok(())
}
