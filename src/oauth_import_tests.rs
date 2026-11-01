use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::config_import_auth::Plan;
use crate::config_import_sources::{ImportBatch, ImportedServer};
use crate::oauth::test_support::{Fixture, Mock};
use crate::oauth::{authorization_header, context, files, store};

fn prepare(fixture: &Fixture, snapshot: Value) -> Result<(Plan, std::path::PathBuf)> {
    let source = fixture.root.join("data/mcporter/credentials.json");
    let authentication = context(&fixture.server)?;
    files::write_json(
        &source,
        &json!({"version":2,"entries":{authentication.identity_key:snapshot}}),
    )?;
    let mut batch = ImportBatch::default();
    batch.servers.insert(
        "synthetic".to_owned(),
        ImportedServer {
            entry: json!({"baseUrl":fixture.server.definition.url,"auth":"oauth",
            "env":{"XDG_DATA_HOME":fixture.root.join("data")}}),
            source: fixture.root.join("mcporter.json"),
        },
    );
    Ok((
        Plan::prepare(
            &mut batch,
            &fixture.server.source,
            &fixture.root.join("home"),
            false,
        )?,
        source,
    ))
}

#[tokio::test]
async fn copied_expired_tokens_refresh_with_the_original_client_and_leave_source_untouched()
-> Result<()> {
    let mock = Mock::new(false).await?;
    let fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    let (plan, source) = prepare(
        &fixture,
        json!({
            "serverName":"synthetic","serverUrl":fixture.server.definition.url,
            "tokens":{"access_token":"synthetic-old","token_type":"Bearer","refresh_token":"synthetic-refresh","expires_at":1,"issuer":mock.origin},
            "clientInfo":{"client_id":"synthetic-client","token_endpoint_auth_method":"none","issuer":mock.origin},
            "discoveryState":mock.discovery(),"authorizationServerUrl":mock.origin,
            "resourceUrl":fixture.server.definition.url
        }),
    )?;
    let original = std::fs::read(&source)?;
    plan.commit()?;
    assert_eq!(mock.token_requests.load(Ordering::SeqCst), 0);
    let authentication = context(&fixture.server)?;
    assert_eq!(
        authorization_header(&authentication).await?,
        Some("Bearer synthetic-new".to_owned())
    );
    assert_eq!(mock.token_requests.load(Ordering::SeqCst), 1);
    assert_eq!(mock.registrations.load(Ordering::SeqCst), 0);
    let snapshot = store::read(&authentication)?;
    assert_eq!(
        snapshot.pointer("/tokens/refresh_token"),
        Some(&json!("synthetic-rotated"))
    );
    let forms = mock
        .forms
        .lock()
        .map_err(|_| anyhow::anyhow!("mock forms poisoned"))?;
    let form = forms.first().context("refresh form")?;
    assert_eq!(
        form.get("client_id").map(String::as_str),
        Some("synthetic-client")
    );
    assert_eq!(
        form.get("refresh_token").map(String::as_str),
        Some("synthetic-refresh")
    );
    assert_eq!(std::fs::read(&source)?, original);
    Ok(())
}

#[test]
fn destination_changed_after_preflight_is_not_overwritten() -> Result<()> {
    let fixture = Fixture::new("https://example.invalid/mcp")?;
    let (plan, _) = prepare(
        &fixture,
        json!({
            "serverName":"synthetic","serverUrl":fixture.server.definition.url,
            "tokens":{"access_token":"synthetic-import","token_type":"Bearer"}
        }),
    )?;
    fixture.valid()?;
    let path = context(&fixture.server)?.vault_path;
    let original = std::fs::read(&path)?;
    assert!(plan.commit().is_err());
    assert_eq!(std::fs::read(&path)?, original);
    Ok(())
}

#[test]
fn runtime_authentication_resolves_only_relevant_configured_environment_values() -> Result<()> {
    let mut fixture = Fixture::new("https://example.invalid/mcp")?;
    fixture.server.raw = json!({
        "auth":"oauth","oauthClientId":"$env:SYNTHETIC_CLIENT",
        "tokenCacheDir":"${SYNTHETIC_CACHE}",
        "env":{"SYNTHETIC_CLIENT":"synthetic-client","SYNTHETIC_CACHE":fixture.root.join("cache"),"UNRELATED":"${UNSET_SECRET}"}
    });
    let authentication = context(&fixture.server)?;
    assert_eq!(
        authentication.client_id.as_deref(),
        Some("synthetic-client")
    );
    assert_eq!(
        authentication.directory_stores.first(),
        Some(&fixture.root.join("cache"))
    );
    assert_eq!(
        fixture.server.raw.get("oauthClientId"),
        Some(&json!("$env:SYNTHETIC_CLIENT"))
    );
    Ok(())
}

#[test]
fn unfinished_authorization_started_after_preflight_is_preserved() -> Result<()> {
    for field in ["state", "codeVerifier", "discoveryState"] {
        let fixture = Fixture::new("https://example.invalid/mcp")?;
        let (plan, _) = prepare(
            &fixture,
            json!({
                "serverName":"synthetic","serverUrl":fixture.server.definition.url,
                "tokens":{"access_token":"synthetic-import","token_type":"Bearer"}
            }),
        )?;
        fixture.seed(json!({field:"synthetic-in-progress"}))?;
        let path = context(&fixture.server)?.vault_path;
        let original = std::fs::read(&path)?;
        assert!(plan.commit().is_err(), "{field}");
        assert_eq!(std::fs::read(path)?, original, "{field}");
    }
    Ok(())
}
