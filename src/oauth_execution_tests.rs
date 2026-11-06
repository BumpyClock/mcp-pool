use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::test_support::{Fixture, Mock};
use super::{HttpAuth, authorization_header, context, files};

#[tokio::test]
async fn refresh_uses_captured_inputs_despite_conflicting_daemon_environment() -> Result<()> {
    for source in ["caller", "configured", "literal", "stored"] {
        let mock = Mock::new(false).await?;
        let mut fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
        let variable = "OAUTH_SYNTHETIC_CLIENT_SECRET";
        fixture.server.raw = json!({
            "auth":"oauth", "oauthClientSecretEnv":variable,
            "oauthTokenEndpointAuthMethod":"client_secret_post"
        });
        if source == "configured" || source == "stored" {
            fixture
                .server
                .raw
                .as_object_mut()
                .context("server entry")?
                .insert("env".to_owned(), json!({variable:"configured-secret"}));
            fixture
                .server
                .definition
                .env
                .insert(variable.to_owned(), "configured-secret".to_owned());
        }
        if source == "literal" {
            let entry = fixture.server.raw.as_object_mut().context("server entry")?;
            entry.remove("oauthClientSecretEnv");
            entry.insert("oauthClientSecret".to_owned(), json!("literal-secret"));
        }
        let mut discovery = mock.discovery();
        *discovery
            .pointer_mut("/authorizationServerMetadata/token_endpoint_auth_methods_supported")
            .context("issuer methods")? = json!(["client_secret_post"]);
        fixture.seed(json!({
            "tokens":{"access_token":"synthetic-old","refresh_token":"synthetic-refresh","token_type":"Bearer","expires_at":1,"issuer":mock.origin},
            "clientInfo":{"client_id":"synthetic-client","token_endpoint_auth_method":"client_secret_post","issuer":mock.origin,"client_secret":(source == "stored").then_some("stored-secret")},
            "authorizationServerUrl":mock.origin,"discoveryState":discovery
        }))?;
        let serialized = fixture.root.join("context.json");
        let status = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "oauth::execution_tests::capture_context_child_process",
            ])
            .env(
                "OAUTH_SYNTHETIC_SERVER",
                serde_json::to_string(&fixture.server)?,
            )
            .env("OAUTH_SYNTHETIC_CONTEXT_PATH", &serialized)
            .env(variable, "caller-secret")
            .kill_on_drop(true)
            .status()
            .await?;
        assert!(status.success());
        let authentication: HttpAuth = serde_json::from_slice(&std::fs::read(serialized)?)?;
        let diagnostic = format!("{authentication:?}");
        for secret in [
            "configured-secret",
            "caller-secret",
            "literal-secret",
            "stored-secret",
        ] {
            assert!(!diagnostic.contains(secret));
        }
        let status = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "oauth::authorization_tests::refresh_child_process",
            ])
            .env(
                "OAUTH_SYNTHETIC_CONTEXT",
                serde_json::to_string(&authentication)?,
            )
            .env(variable, "daemon-secret")
            .kill_on_drop(true)
            .status()
            .await?;
        assert!(status.success());
        let forms = mock
            .forms
            .lock()
            .map_err(|_| anyhow::anyhow!("mock form lock poisoned"))?;
        let form = forms.first().context("refresh form missing")?;
        assert_eq!(forms.len(), 1);
        assert_eq!(
            form.get("client_secret").map(String::as_str),
            Some(match source {
                "configured" => "configured-secret",
                "literal" => "literal-secret",
                "stored" => "stored-secret",
                _ => "caller-secret",
            })
        );
    }
    Ok(())
}

#[test]
fn capture_context_child_process() -> Result<()> {
    let Ok(server) = std::env::var("OAUTH_SYNTHETIC_SERVER") else {
        return Ok(());
    };
    let server = serde_json::from_str(&server)?;
    let authentication = context(&server)?;
    files::write_json(
        &std::path::PathBuf::from(std::env::var("OAUTH_SYNTHETIC_CONTEXT_PATH")?),
        &serde_json::to_value(authentication)?,
    )
}

#[tokio::test]
async fn legacy_auth_context_defaults_preserve_cached_and_registered_credentials() -> Result<()> {
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.valid()?;
    assert_eq!(
        super::credential_status(&fixture.server)?.get("bindingValid"),
        Some(&json!(true))
    );
    let mut wire = serde_json::to_value(context(&fixture.server)?)?;
    wire.as_object_mut()
        .context("authentication object")?
        .remove("execution");
    let authentication: HttpAuth = serde_json::from_value(wire)?;
    assert_eq!(
        authorization_header(&authentication).await?.as_deref(),
        Some("Bearer synthetic-current")
    );
    let info = json!({"client_id":"synthetic-client","client_secret":"registered-secret"});
    assert_eq!(
        authentication.client_secret(&info)?,
        Some("registered-secret")
    );
    Ok(())
}

#[test]
fn secret_environment_names_follow_platform_semantics_and_stored_values_win() -> Result<()> {
    let mut fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    let lower = format!(
        "oauth_synthetic_{}",
        oauth2::CsrfToken::new_random()
            .secret()
            .to_ascii_lowercase()
    );
    fixture.server.raw = json!({"auth":"oauth", "oauthClientSecretEnv":lower.to_ascii_uppercase()});
    fixture
        .server
        .definition
        .env
        .insert(lower, "captured-secret".to_owned());
    let authentication = context(&fixture.server)?;
    let result = authentication.client_secret(&Value::Null);
    if cfg!(windows) {
        assert_eq!(result?, Some("captured-secret"));
    } else {
        assert!(result.is_err());
    }
    let info = json!({"client_secret":"registered-secret"});
    assert_eq!(
        authentication.client_secret(&info)?,
        Some("registered-secret")
    );
    Ok(())
}
