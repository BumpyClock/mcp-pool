use std::sync::atomic::Ordering;

use anyhow::Result;
use reqwest::Url;
use serde_json::{Value, json};

use super::test_support::{Fixture, Mock};
use super::{
    AuthOptions, authorization_header, authorize, context, credential_status, files, flow, prepare,
    store, vault_clear, vault_set,
};

#[tokio::test]
async fn cached_credentials_reuse_without_discovery_or_login() -> Result<()> {
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.valid()?;
    let before = std::fs::read(context(&fixture.server)?.vault_path)?;
    let definition = prepare(&fixture.server, false).await?;
    let authentication = definition
        .auth
        .ok_or_else(|| anyhow::anyhow!("auth missing"))?;
    assert!(
        authorization_header(&authentication).await?.as_deref() == Some("Bearer synthetic-current")
    );
    let response = authorize(
        &fixture.server,
        AuthOptions {
            reset: false,
            no_browser: true,
            json: true,
        },
    )
    .await?;
    assert!(response.get("reused") == Some(&json!(true)));
    assert!(
        std::fs::read(&authentication.vault_path)? == before,
        "cached reads must not rewrite store"
    );
    let rendered = serde_json::to_string(&credential_status(&fixture.server)?)?;
    assert!(!rendered.contains("synthetic-current"));
    assert!(!format!("{authentication:?}").contains("synthetic.invalid"));
    Ok(())
}

#[tokio::test]
async fn missing_oauth_is_actionable_and_no_oauth_keeps_valid_cached_tokens() -> Result<()> {
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    let error = prepare(&fixture.server, false)
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("expected auth failure"))?;
    assert!(error.to_string().contains("mcp-pool auth"));
    assert!(prepare(&fixture.server, true).await.is_err());
    assert!(
        !context(&fixture.server)?.vault_path.exists(),
        "read must not create vault"
    );
    fixture.valid()?;
    assert!(prepare(&fixture.server, true).await?.auth.is_some());
    Ok(())
}

#[tokio::test]
async fn read_only_validation_reuses_valid_tokens_and_blocks_all_writes() -> Result<()> {
    let mut fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.valid()?;
    let path = context(&fixture.server)?.vault_path;
    let before = std::fs::read(&path)?;
    fixture
        .server
        .definition
        .env
        .insert("MCP_POOL_CREDENTIALS_READ_ONLY".to_owned(), "1".to_owned());
    let definition = prepare(&fixture.server, false).await?;
    let mut authentication = definition
        .auth
        .ok_or_else(|| anyhow::anyhow!("auth missing"))?;
    assert!(authorization_header(&authentication).await?.is_some());
    assert!(
        vault_set(
            &fixture.server,
            &json!({"access_token":"synthetic-replacement","token_type":"Bearer"})
        )
        .is_err()
    );
    assert!(vault_clear(&fixture.server).is_err());
    assert!(
        authorize(
            &fixture.server,
            AuthOptions {
                reset: false,
                no_browser: true,
                json: true
            }
        )
        .await
        .is_err()
    );
    authentication.read_only = false;
    let old = json!({"tokens":{"access_token":"synthetic-expired","token_type":"Bearer","expires_at":1,"refresh_token":"synthetic-refresh"}});
    store::save(&authentication, &old, None)?;
    authentication.read_only = true;
    let expired_before = std::fs::read(&path)?;
    assert!(authorization_header(&authentication).await.is_err());
    assert!(
        std::fs::read(&path)? == expired_before,
        "expired validation must not refresh or rewrite"
    );
    assert!(!before.is_empty());
    Ok(())
}

#[test]
fn vault_v1_identity_and_v2_binding_are_preserved() -> Result<()> {
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    let authentication = context(&fixture.server)?;
    let expected = r#"{"name":"synthetic","url":"https://synthetic.invalid/mcp","command":null}"#;
    assert!(authentication.identity_key == format!("synthetic|{}", files::digest(expected)));
    let mut entries = serde_json::Map::new();
    entries.insert(authentication.identity_key.clone(), json!({
        "serverName":"synthetic","serverUrl":"https://synthetic.invalid/mcp","updatedAt":"2026-01-01T00:00:00.000Z",
        "tokens":{"access_token":"synthetic-v1","token_type":"Bearer"}
    }));
    files::write_json(
        &authentication.vault_path,
        &json!({"version":1,"entries":entries}),
    )?;
    assert!(store::read(&authentication)?.get("tokens").is_some());
    let mut switched = authentication.clone();
    switched.server_url = "https://other.invalid/mcp".to_owned();
    assert!(store::read(&switched)?.get("tokens").is_none());
    assert!(
        store::validate_binding(
            &authentication,
            &json!({
                "tokens":{"issuer":"https://issuer-one.invalid"},
                "clientInfo":{"issuer":"https://issuer-two.invalid"}
            })
        )
        .is_err()
    );
    assert!(
        store::validate_binding(
            &authentication,
            &json!({"resourceUrl":"https://other.invalid/mcp"})
        )
        .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn refresh_is_serialized_and_rotating_generation_is_persisted() -> Result<()> {
    let mock = Mock::new(false).await?;
    let fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    fixture.seed(json!({
        "tokens":{"access_token":"synthetic-old","refresh_token":"synthetic-refresh","token_type":"Bearer","expires_at":1,"issuer":mock.origin},
        "clientInfo":{"client_id":"synthetic-client","token_endpoint_auth_method":"none","issuer":mock.origin},
        "authorizationServerUrl":mock.origin,"resourceUrl":format!("{}/mcp",mock.origin),"discoveryState":mock.discovery()
    }))?;
    let authentication = context(&fixture.server)?;
    let mut tasks = Vec::new();
    for _iteration in 0..12 {
        let authentication = authentication.clone();
        tasks.push(tokio::spawn(async move {
            authorization_header(&authentication).await
        }));
    }
    for task in tasks {
        assert!(task.await??.as_deref() == Some("Bearer synthetic-new"));
    }
    assert!(
        mock.token_requests.load(Ordering::SeqCst) == 1,
        "refresh grant must be redeemed exactly once"
    );
    let snapshot = store::read(&authentication)?;
    assert!(
        snapshot
            .get("tokens")
            .and_then(|tokens| tokens.get("refresh_token"))
            == Some(&json!("synthetic-rotated"))
    );
    assert!(
        snapshot
            .get("tokens")
            .and_then(|tokens| tokens.get("expires_at"))
            .and_then(Value::as_u64)
            .is_some_and(|value| value > files::now())
    );
    Ok(())
}

#[tokio::test]
async fn rejected_refresh_preserves_persisted_credentials() -> Result<()> {
    let mock = Mock::new(true).await?;
    let fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    fixture.seed(json!({
        "tokens":{"access_token":"synthetic-old","refresh_token":"synthetic-refresh","token_type":"Bearer","expires_at":1,"issuer":mock.origin},
        "clientInfo":{"client_id":"synthetic-client","token_endpoint_auth_method":"none","issuer":mock.origin},
        "authorizationServerUrl":mock.origin,"discoveryState":mock.discovery()
    }))?;
    let authentication = context(&fixture.server)?;
    let before = std::fs::read(&authentication.vault_path)?;
    assert!(authorization_header(&authentication).await.is_err());
    assert!(
        std::fs::read(&authentication.vault_path)? == before,
        "refresh failures must not clear or rewrite credentials"
    );
    Ok(())
}

#[tokio::test]
async fn explicit_authorization_discovers_registers_pkce_and_completes_loopback() -> Result<()> {
    let mock = Mock::new(false).await?;
    let fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    let authorization = mock.authorization.clone();
    let issuer = mock.origin.clone();
    let result = flow::authorize_with_observer(
        &fixture.server,
        AuthOptions {
            reset: false,
            no_browser: true,
            json: true,
        },
        None,
        move |url, redirect| async move {
            let state = url
                .query_pairs()
                .find(|(key, _)| key == "state")
                .map(|(_, value)| value.into_owned())
                .ok_or_else(|| anyhow::anyhow!("state missing"))?;
            assert!(
                url.query_pairs()
                    .any(|(key, value)| key == "code_challenge_method" && value == "S256")
            );
            *authorization
                .lock()
                .map_err(|_| anyhow::anyhow!("mock lock poisoned"))? = Some(url);
            let mut callback = redirect;
            callback
                .query_pairs_mut()
                .append_pair("state", &state)
                .append_pair("code", "synthetic-code")
                .append_pair("iss", &issuer);
            tokio::spawn(async move {
                match reqwest::get(callback).await {
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
    .await?;
    assert!(result.get("status") == Some(&json!("authenticated")));
    assert!(mock.registrations.load(Ordering::SeqCst) == 1);
    assert!(mock.token_requests.load(Ordering::SeqCst) == 1);
    let snapshot = store::read(&context(&fixture.server)?)?;
    assert!(
        snapshot
            .get("clientInfo")
            .and_then(|value| value.get("client_id"))
            == Some(&json!("synthetic-client"))
    );
    assert!(snapshot.get("state").is_some_and(Value::is_null));
    assert!(snapshot.get("codeVerifier").is_some_and(Value::is_null));
    assert!(
        mock.forms
            .lock()
            .map_err(|_| anyhow::anyhow!("mock forms lock poisoned"))?
            .first()
            .is_some_and(|form| form.get("code_verifier").is_some())
    );
    Ok(())
}

#[test]
fn callback_validates_state_issuer_and_duplicate_parameters() -> Result<()> {
    let callback = |query: &str| -> Result<Url> {
        Ok(Url::parse(&format!("http://127.0.0.1/callback?{query}"))?)
    };
    let issuer = "https://synthetic-issuer.invalid";
    assert!(flow::callback_query(&callback("state=synthetic-state&code=synthetic-code&iss=https%3A%2F%2Fsynthetic-issuer.invalid")?, "synthetic-state", issuer, true).is_ok());
    assert!(
        flow::callback_query(
            &callback("state=wrong&code=synthetic-code")?,
            "synthetic-state",
            issuer,
            false
        )
        .is_err()
    );
    assert!(
        flow::callback_query(
            &callback("state=synthetic-state&code=synthetic-code")?,
            "synthetic-state",
            issuer,
            true
        )
        .is_err()
    );
    assert!(
        flow::callback_query(
            &callback("state=synthetic-state&code=synthetic-code&iss=https%3A%2F%2Fother.invalid")?,
            "synthetic-state",
            issuer,
            true
        )
        .is_err()
    );
    assert!(
        flow::callback_query(
            &callback("state=synthetic-state&state=wrong&code=synthetic-code")?,
            "synthetic-state",
            issuer,
            false
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn explicit_directory_store_precedence_and_clear_are_bounded() -> Result<()> {
    let mut fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.server.raw = json!({"auth":"oauth","tokenCacheDir":fixture.root.join("cache")});
    fixture.valid()?;
    let authentication = context(&fixture.server)?;
    let directory = authentication
        .directory_stores
        .first()
        .ok_or_else(|| anyhow::anyhow!("directory missing"))?;
    files::atomic_write(&directory.join("unrelated.txt"), b"preserve")?;
    files::write_json(
        &directory.join("tokens.json"),
        &json!({"access_token":"synthetic-directory","token_type":"Bearer"}),
    )?;
    assert!(
        store::read(&authentication)?
            .get("tokens")
            .and_then(|tokens| tokens.get("access_token"))
            == Some(&json!("synthetic-directory"))
    );
    vault_clear(&fixture.server)?;
    assert!(directory.join("unrelated.txt").exists());
    assert!(!directory.join("tokens.json").exists());
    Ok(())
}

#[test]
fn stale_generation_cannot_overwrite_a_newer_vault_write() -> Result<()> {
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.valid()?;
    let authentication = context(&fixture.server)?;
    let original = store::read(&authentication)?;
    vault_set(
        &fixture.server,
        &json!({"access_token":"synthetic-fresh","token_type":"Bearer"}),
    )?;
    assert!(
        store::save(
            &authentication,
            &json!({"tokens":{"access_token":"synthetic-stale","token_type":"Bearer"}}),
            Some(&original)
        )
        .is_err()
    );
    assert!(
        store::read(&authentication)?
            .get("tokens")
            .and_then(|tokens| tokens.get("access_token"))
            == Some(&json!("synthetic-fresh"))
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn persisted_credentials_have_private_unix_permissions() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.valid()?;
    let path = context(&fixture.server)?.vault_path;
    assert!(std::fs::metadata(&path)?.permissions().mode() & 0o777 == 0o600);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("vault parent missing"))?;
    assert!(std::fs::metadata(parent)?.permissions().mode() & 0o777 == 0o700);
    Ok(())
}

#[cfg(windows)]
#[test]
fn persisted_credentials_have_protected_windows_dacl() -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, SE_DACL_PROTECTED,
    };
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.valid()?;
    let path = context(&fixture.server)?.vault_path;
    for path in [
        &path,
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("vault parent missing"))?,
    ] {
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut descriptor = std::ptr::null_mut();
        let result = unsafe {
            GetNamedSecurityInfoW(
                wide.as_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if result != 0 {
            return Err(anyhow::anyhow!(
                "could not inspect synthetic credential ACL"
            ));
        }
        let mut control = 0_u16;
        let mut revision = 0_u32;
        let inspected =
            unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
        unsafe {
            LocalFree(descriptor);
        }
        assert!(
            inspected != 0 && control & SE_DACL_PROTECTED != 0,
            "credential DACL must not inherit broad permissions"
        );
    }
    Ok(())
}
