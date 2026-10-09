use std::sync::atomic::Ordering;

use anyhow::Result;
use reqwest::Url;
use serde_json::{Value, json};

use super::test_support::{Fixture, Mock};
use super::{AuthOptions, HttpAuth, authorization_header, context, files, flow, store};

async fn complete(fixture: &Fixture, mock: &Mock) -> Result<Value> {
    let authorization = mock.authorization.clone();
    let issuer = mock.origin.clone();
    flow::authorize_with_observer(
        &fixture.server,
        AuthOptions {
            reset: false,
            no_browser: true,
            json: true,
        },
        None,
        move |url, mut redirect| async move {
            let state = url
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned())
                .ok_or_else(|| anyhow::anyhow!("state missing"))?;
            *authorization
                .lock()
                .map_err(|_| anyhow::anyhow!("mock lock poisoned"))? = Some(url);
            redirect
                .query_pairs_mut()
                .append_pair("state", &state)
                .append_pair("code", "synthetic-code")
                .append_pair("iss", &issuer);
            tokio::spawn(async move {
                match reqwest::get(redirect).await {
                    Ok(response) => assert!(
                        response.status().is_success(),
                        "synthetic callback rejected"
                    ),
                    Err(_) => panic!("synthetic callback failed"),
                }
            });
            Ok(())
        },
    )
    .await
}

#[tokio::test]
async fn preconfigured_public_client_authorizes_without_dynamic_registration() -> Result<()> {
    let mock = Mock::new(false).await?;
    let mut fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    fixture.server.raw = json!({"auth":"oauth","oauthClientId":"synthetic-client"});
    assert!(complete(&fixture, &mock).await?.get("status") == Some(&json!("authenticated")));
    assert!(mock.registrations.load(Ordering::SeqCst) == 0);
    assert!(mock.token_requests.load(Ordering::SeqCst) == 1);
    Ok(())
}

#[tokio::test]
async fn existing_registered_client_and_redirect_are_reused_during_reauthorization() -> Result<()> {
    let mock = Mock::new(false).await?;
    let fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    let available = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let redirect = format!("http://{}/callback", available.local_addr()?);
    drop(available);
    fixture.seed(json!({
        "tokens":{"access_token":"synthetic-expired","token_type":"Bearer","expires_at":1,"issuer":mock.origin},
        "clientInfo":{"client_id":"synthetic-client","issuer":mock.origin,"redirect_uris":[redirect],"token_endpoint_auth_method":"none","__mcporter_client_generation":"synthetic-original-registration"},
        "authorizationServerUrl":mock.origin,"discoveryState":mock.discovery()
    }))?;
    assert!(complete(&fixture, &mock).await?.get("status") == Some(&json!("authenticated")));
    assert!(mock.registrations.load(Ordering::SeqCst) == 0);
    let snapshot = store::read(&context(&fixture.server)?)?;
    assert!(
        snapshot
            .get("clientInfo")
            .and_then(|info| info.get("__mcporter_client_generation"))
            == Some(&json!("synthetic-original-registration")),
        "must retain existing registered client identity"
    );
    Ok(())
}

#[tokio::test]
async fn headless_authorization_timeout_releases_all_transaction_locks() -> Result<()> {
    let mock = Mock::new(false).await?;
    let mut fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    fixture.server.definition.timeout_ms = Some(1000);
    let result = flow::authorize_with_observer(
        &fixture.server,
        AuthOptions {
            reset: false,
            no_browser: true,
            json: true,
        },
        None,
        |_, _| async { Ok(()) },
    )
    .await;
    assert!(
        result.is_err(),
        "headless authorization must have a deadline"
    );
    assert!(mock.token_requests.load(Ordering::SeqCst) == 0);
    let authentication = context(&fixture.server)?;
    let _locks = store::transaction_async(&authentication).await?;
    assert!(store::read(&authentication)?.get("tokens").is_none());
    Ok(())
}

#[tokio::test]
async fn rotating_refresh_is_serialized_across_processes() -> Result<()> {
    let mock = Mock::new(false).await?;
    let fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    fixture.seed(json!({
        "tokens":{"access_token":"synthetic-old","refresh_token":"synthetic-refresh","token_type":"Bearer","expires_at":1,"issuer":mock.origin},
        "clientInfo":{"client_id":"synthetic-client","token_endpoint_auth_method":"none","issuer":mock.origin},
        "authorizationServerUrl":mock.origin,"resourceUrl":format!("{}/mcp",mock.origin),"discoveryState":mock.discovery()
    }))?;
    let authentication = serde_json::to_string(&context(&fixture.server)?)?;
    let executable = std::env::current_exe()?;
    let mut children = Vec::new();
    for _iteration in 0..4 {
        children.push(
            tokio::process::Command::new(&executable)
                .args([
                    "--exact",
                    "oauth::authorization_tests::refresh_child_process",
                ])
                .env("OAUTH_SYNTHETIC_CONTEXT", &authentication)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()?,
        );
    }
    for mut child in children {
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(15), child.wait())
                .await??
                .success(),
            "synthetic refresh subprocess failed"
        );
    }
    assert!(
        mock.token_requests.load(Ordering::SeqCst) == 1,
        "cross-process refresh must redeem once"
    );
    Ok(())
}

#[tokio::test]
async fn refresh_child_process() -> Result<()> {
    let Ok(value) = std::env::var("OAUTH_SYNTHETIC_CONTEXT") else {
        return Ok(());
    };
    let authentication: HttpAuth = serde_json::from_str(&value)?;
    if std::env::var("OAUTH_SYNTHETIC_READ_ONLY").is_ok() {
        assert!(authorization_header(&authentication).await.is_err());
    } else {
        assert!(
            authorization_header(&authentication).await?.as_deref() == Some("Bearer synthetic-new")
        );
    }
    Ok(())
}

#[tokio::test]
async fn read_only_environment_guard_prevents_refresh_even_in_new_process() -> Result<()> {
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.seed(json!({"tokens":{"access_token":"synthetic-expired","refresh_token":"synthetic-refresh","token_type":"Bearer","expires_at":1}}))?;
    let authentication = context(&fixture.server)?;
    let original = std::fs::read(&authentication.vault_path)?;
    let status = tokio::process::Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "oauth::authorization_tests::refresh_child_process",
        ])
        .env(
            "OAUTH_SYNTHETIC_CONTEXT",
            serde_json::to_string(&authentication)?,
        )
        .env("OAUTH_SYNTHETIC_READ_ONLY", "1")
        .env("MCP_POOL_CREDENTIALS_READ_ONLY", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status()
        .await?;
    assert!(status.success());
    assert!(
        std::fs::read(&authentication.vault_path)? == original,
        "validation process must not touch vault"
    );
    Ok(())
}

#[test]
fn discovery_rejects_unadvertised_issuer_and_remote_plaintext() -> Result<()> {
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    let invalid = json!({
        "authorizationServerUrl":"https://issuer.invalid",
        "resourceMetadata":{"resource":"https://synthetic.invalid/mcp","authorization_servers":["https://other.invalid"]},
        "authorizationServerMetadata":{"issuer":"https://issuer.invalid","authorization_endpoint":"https://issuer.invalid/authorize","token_endpoint":"https://issuer.invalid/token"}
    });
    assert!(super::protocol::validate_discovery(&context(&fixture.server)?, &invalid).is_err());
    assert!(super::checked_url("http://remote.invalid/token").is_err());
    assert!(super::checked_url("https://user:secret@synthetic.invalid/token").is_err());
    assert!(files::timestamp().ends_with(".000Z"));
    assert!(Url::parse("http://127.0.0.1/callback").is_ok());
    Ok(())
}

#[tokio::test]
async fn explicit_stdio_helper_is_owned_and_success_is_verified() -> Result<()> {
    let mut fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.server.definition.url.clear();
    configure_stdio_helper(&mut fixture, "success");
    let response = super::authorize(
        &fixture.server,
        AuthOptions {
            reset: false,
            no_browser: true,
            json: true,
        },
    )
    .await?;
    assert!(response.get("status") == Some(&json!("helper_completed")));
    Ok(())
}

#[tokio::test]
async fn failed_and_timed_out_stdio_helpers_do_not_report_authorization_success() -> Result<()> {
    let mut fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.server.definition.url.clear();
    fixture.server.definition.timeout_ms = Some(1000);
    for mode in ["failure", "timeout"] {
        configure_stdio_helper(&mut fixture, mode);
        let result = super::authorize(
            &fixture.server,
            AuthOptions {
                reset: false,
                no_browser: true,
                json: true,
            },
        )
        .await;
        assert!(
            result.is_err(),
            "failed helper must not be accepted as success"
        );
    }
    Ok(())
}

fn configure_stdio_helper(fixture: &mut Fixture, mode: &str) {
    #[cfg(windows)]
    let (command, arguments, script) = (
        "powershell.exe",
        vec!["-NoProfile".to_owned(), "-Command".to_owned()],
        match mode {
            "success" => "exit 0",
            "failure" => "exit 7",
            _ => "Start-Sleep -Seconds 30",
        },
    );
    #[cfg(unix)]
    let (command, arguments, script) = (
        "sh",
        vec!["-c".to_owned()],
        match mode {
            "success" => "exit 0",
            "failure" => "exit 7",
            _ => "sleep 30",
        },
    );
    fixture.server.definition.command = command.to_owned();
    fixture.server.definition.args = arguments;
    fixture.server.raw = json!({"oauthCommand":{"args":[script]}});
}

#[tokio::test]
async fn explicit_oauth_timeout_overrides_only_the_attempt_deadline() -> Result<()> {
    let mock = Mock::new(false).await?;
    let fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    let context_before = serde_json::to_value(context(&fixture.server)?)?;
    let environment_before = std::env::var("MCPORTER_OAUTH_TIMEOUT_MS");
    let started = tokio::time::Instant::now();
    let result = flow::authorize_with_observer(
        &fixture.server,
        AuthOptions {
            reset: false,
            no_browser: true,
            json: true,
        },
        Some(100),
        |_, _| async { Ok(()) },
    )
    .await;
    assert!(
        result.is_err(),
        "uncompleted authorization must reach the per-invocation deadline"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "configured default timeout must not replace explicit timeout"
    );
    assert!(
        serde_json::to_value(context(&fixture.server)?)? == context_before,
        "authorization deadlines must not change configured authentication context"
    );
    assert!(std::env::var("MCPORTER_OAUTH_TIMEOUT_MS") == environment_before);
    assert!(mock.token_requests.load(Ordering::SeqCst) == 0);
    Ok(())
}

#[tokio::test]
async fn explicit_stdio_oauth_timeout_is_not_clamped_to_configured_default() -> Result<()> {
    let mut fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.server.definition.url.clear();
    configure_stdio_helper(&mut fixture, "timeout");
    let started = tokio::time::Instant::now();
    let result = super::authorize_with_timeout(
        &fixture.server,
        AuthOptions {
            reset: false,
            no_browser: true,
            json: true,
        },
        Some(100),
    )
    .await;
    assert!(result.is_err());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "stdio auth must apply the explicit timeout and verify retirement"
    );
    Ok(())
}

#[tokio::test]
async fn zero_oauth_timeout_fails_before_any_credential_mutation() -> Result<()> {
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    let path = context(&fixture.server)?.vault_path;
    let result = super::authorize_with_timeout(
        &fixture.server,
        AuthOptions {
            reset: true,
            no_browser: true,
            json: true,
        },
        Some(0),
    )
    .await;
    assert!(result.is_err());
    assert!(!path.exists());
    Ok(())
}
