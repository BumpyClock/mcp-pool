use std::sync::atomic::Ordering;

use anyhow::Result;
use serde_json::{Value, json};

use super::test_support::{Fixture, Mock};
use super::{authorization_header, context, files, reconcile, store};

fn expired_payload(mock: &Mock) -> Value {
    json!({
        "tokens":{"access_token":"synthetic-old","refresh_token":"synthetic-old-refresh","token_type":"Bearer","expires_at":1,"issuer":mock.origin},
        "clientInfo":{"client_id":"synthetic-client","token_endpoint_auth_method":"none","issuer":mock.origin},
        "authorizationServerUrl":mock.origin,"resourceUrl":format!("{}/mcp",mock.origin),"discoveryState":mock.discovery()
    })
}

#[test]
fn refresh_lock_filenames_match_cross_application_contract() -> Result<()> {
    let fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    let mut authentication = context(&fixture.server)?;
    let root = authentication
        .vault_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("fixture vault has no parent"))?
        .join("refresh-locks");
    for (name, filename) in [
        ("a / b", "a_b-4f0c00ec1e063ce8.lock"),
        ("a__ / b", "a___b-cbe83c16cdbe3d02.lock"),
        ("a|b", "a-4131a00068f76a3f.lock"),
        ("._-/ 🦀 \\ a", "a-d79f18cf9e8cd718.lock"),
        ("/ : 🦀", "75b1c8e8d4acc21f.lock"),
        (
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa / b",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-375bd19a3f045d5c.lock",
        ),
    ] {
        authentication.server_name = name.to_owned();
        authentication.identity_key = format!("{name}|0123456789abcdef");
        let locks = store::transaction(&authentication)?;
        let path = root.join(filename);
        assert!(path.is_file(), "reference lock filename must be acquired");
        let error = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .err()
            .ok_or_else(|| {
                anyhow::anyhow!("cross-application lock did not exclude a second owner")
            })?;
        assert!(error.kind() == std::io::ErrorKind::AlreadyExists);
        drop(locks);
        assert!(!path.exists(), "transaction must release the shared lock");
    }
    Ok(())
}

#[test]
fn directory_refresh_lock_label_uses_canonical_token_identity() -> Result<()> {
    let mut fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    let cache = fixture.root.join("canonical cache");
    std::fs::create_dir_all(cache.join("unused"))?;
    fixture.server.raw = json!({
        "auth":"oauth","tokenCacheDir":cache.join("unused").join("..")
    });
    let authentication = context(&fixture.server)?;
    let token_path = std::fs::canonicalize(&cache)?.join("tokens.json");
    let identity = format!("dir:{}", files::portable_path(&token_path));
    let expected = authentication
        .vault_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("fixture vault has no parent"))?
        .join("refresh-locks")
        .join(format!("canonical_cache-{}.lock", files::digest(&identity)));
    let locks = store::transaction(&authentication)?;
    assert!(
        expected.is_file(),
        "the label must name the canonical store, not its configured spelling"
    );
    let error = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&expected)
        .err()
        .ok_or_else(|| anyhow::anyhow!("canonical store lock did not exclude a second owner"))?;
    assert!(error.kind() == std::io::ErrorKind::AlreadyExists);
    drop(locks);
    assert!(!expected.exists());
    Ok(())
}

#[tokio::test]
async fn committed_rotated_generation_without_expiry_beats_stale_primary_store() -> Result<()> {
    let mock = Mock::new(false).await?;
    let mut fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    fixture.server.raw = json!({"auth":"oauth","tokenCacheDir":fixture.root.join("cache")});
    fixture.seed(expired_payload(&mock))?;
    let authentication = context(&fixture.server)?;
    let previous = store::read(&authentication)?;
    let mut entry = previous.clone();
    if let Some(object) = entry.as_object_mut() {
        object.insert("serverName".to_owned(), json!(authentication.server_name));
        object.insert("serverUrl".to_owned(), json!(authentication.server_url));
        object.insert("tokens".to_owned(), json!({
            "access_token":"synthetic-rotated-without-expiry","refresh_token":"synthetic-rotated-refresh",
            "token_type":"Bearer","issuer":mock.origin
        }));
    }
    reconcile::stamp(&mut entry, &previous, std::slice::from_ref(&previous))?;
    let mut vault = files::read_json(&authentication.vault_path)?
        .ok_or_else(|| anyhow::anyhow!("fixture vault missing"))?;
    let entries = vault
        .get_mut("entries")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("fixture entries missing"))?;
    entries.insert(authentication.identity_key.clone(), entry);
    files::write_json(&authentication.vault_path, &vault)?;
    assert!(
        authorization_header(&authentication).await?.as_deref()
            == Some("Bearer synthetic-rotated-without-expiry")
    );
    assert!(
        mock.token_requests.load(Ordering::SeqCst) == 0,
        "the stale refresh token must never be redeemed"
    );
    Ok(())
}

#[cfg(windows)]
#[tokio::test]
async fn failed_primary_atomic_replace_preserves_new_vault_commit_without_expiry() -> Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    let mock = Mock::new(false).await?;
    let mut fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    fixture.server.raw = json!({"auth":"oauth","tokenCacheDir":fixture.root.join("cache")});
    fixture.seed(expired_payload(&mock))?;
    let authentication = context(&fixture.server)?;
    let previous = store::read(&authentication)?;
    let directory = authentication
        .directory_stores
        .first()
        .ok_or_else(|| anyhow::anyhow!("fixture cache missing"))?;
    let path = directory.join("tokens.json");
    let original = std::fs::read(&path)?;
    let deny_replace = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1)
        .open(&path)?;
    let result = store::save(
        &authentication,
        &json!({
            "tokens":{"access_token":"synthetic-rotated-without-expiry","refresh_token":"synthetic-rotated-refresh","token_type":"Bearer","issuer":mock.origin}
        }),
        Some(&previous),
    );
    drop(deny_replace);
    assert!(
        result.is_err(),
        "the synthetic file handle must reject primary replacement"
    );
    assert!(std::fs::read(&path)? == original);
    assert!(
        authorization_header(&authentication).await?.as_deref()
            == Some("Bearer synthetic-rotated-without-expiry")
    );
    assert!(mock.token_requests.load(Ordering::SeqCst) == 0);
    Ok(())
}

#[tokio::test]
async fn different_token_generation_cannot_supply_its_client_registration() -> Result<()> {
    let mock = Mock::new(false).await?;
    let mut fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    fixture.server.raw = json!({"auth":"oauth","tokenCacheDir":fixture.root.join("cache")});
    fixture.seed(expired_payload(&mock))?;
    let authentication = context(&fixture.server)?;
    let directory = authentication
        .directory_stores
        .first()
        .ok_or_else(|| anyhow::anyhow!("fixture cache missing"))?;
    files::write_json(
        &directory.join("tokens.json"),
        &json!({
            "access_token":"synthetic-client-a-token","refresh_token":"synthetic-old-refresh",
            "token_type":"Bearer","expires_at":1,"issuer":mock.origin
        }),
    )?;
    std::fs::remove_file(directory.join("client.json"))?;
    let snapshot = store::read(&authentication)?;
    assert!(
        snapshot.get("clientInfo").is_none(),
        "registration must not be borrowed from another token generation"
    );
    assert!(authorization_header(&authentication).await.is_err());
    assert!(
        mock.token_requests.load(Ordering::SeqCst) == 0,
        "client-A token must not be refreshed using client-B registration"
    );
    Ok(())
}

#[test]
fn ambiguous_unordered_rotating_generations_fail_closed() -> Result<()> {
    let candidates = [
        json!({"tokens":{"access_token":"synthetic-a","refresh_token":"synthetic-refresh-a","token_type":"Bearer","expires_at":1}}),
        json!({"tokens":{"access_token":"synthetic-b","refresh_token":"synthetic-refresh-b","token_type":"Bearer","expires_at":9999999999_u64}}),
    ];
    assert!(
        reconcile::select(&candidates).is_err(),
        "expiry is not proof of refresh commit ordering"
    );
    Ok(())
}

#[test]
fn exact_token_generation_can_recover_its_missing_registration() -> Result<()> {
    let tokens = json!({"access_token":"synthetic","refresh_token":"synthetic-refresh","token_type":"Bearer"});
    let client = json!({"client_id":"synthetic-client"});
    let selected = reconcile::select(&[
        json!({"tokens":tokens}),
        json!({"tokens":tokens,"clientInfo":client}),
    ])?;
    assert!(selected.get("clientInfo") == Some(&client));
    Ok(())
}

#[test]
fn mutated_registration_cannot_use_another_generation_binding() -> Result<()> {
    let mut entry = json!({"tokens":{"access_token":"synthetic","token_type":"Bearer"},"clientInfo":{"client_id":"synthetic-client-a"}});
    reconcile::stamp(&mut entry, &json!({}), &[])?;
    if let Some(object) = entry.as_object_mut() {
        object.insert(
            "clientInfo".to_owned(),
            json!({"client_id":"synthetic-client-b"}),
        );
    }
    assert!(reconcile::select(&[entry]).is_err());
    Ok(())
}

#[tokio::test]
async fn caller_guard_serializes_into_request_for_an_unguarded_worker() -> Result<()> {
    let mock = Mock::new(false).await?;
    let fixture = Fixture::new(&format!("{}/mcp", mock.origin))?;
    let mut valid = expired_payload(&mock);
    if let Some(tokens) = valid.get_mut("tokens").and_then(Value::as_object_mut) {
        tokens.insert("expires_at".to_owned(), json!(files::now() + 3600));
    }
    fixture.seed(valid)?;
    let request_path = fixture.root.join("guarded-request.json");
    let executable = std::env::current_exe()?;
    let prepared = tokio::process::Command::new(&executable)
        .args(["--exact", "oauth::store_tests::caller_guard_prepare_child"])
        .env(
            "OAUTH_SYNTHETIC_SERVER",
            serde_json::to_string(&fixture.server)?,
        )
        .env("OAUTH_SYNTHETIC_REQUEST_PATH", &request_path)
        .env("MCP_POOL_CREDENTIALS_READ_ONLY", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status()
        .await?;
    assert!(prepared.success());
    let request = files::read_json(&request_path)?
        .ok_or_else(|| anyhow::anyhow!("guarded request missing"))?;
    let authentication: super::HttpAuth = serde_json::from_value(request.clone())?;
    assert!(authentication.read_only);
    fixture.seed(expired_payload(&mock))?;
    let original = std::fs::read(&authentication.vault_path)?;
    let worker = tokio::process::Command::new(&executable)
        .args([
            "--exact",
            "oauth::authorization_tests::refresh_child_process",
        ])
        .env("OAUTH_SYNTHETIC_CONTEXT", serde_json::to_string(&request)?)
        .env("OAUTH_SYNTHETIC_READ_ONLY", "1")
        .env_remove("MCP_POOL_CREDENTIALS_READ_ONLY")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .status()
        .await?;
    assert!(
        worker.success(),
        "the unguarded worker must enforce serialized request policy"
    );
    assert!(mock.token_requests.load(Ordering::SeqCst) == 0);
    assert!(std::fs::read(&authentication.vault_path)? == original);
    Ok(())
}

#[tokio::test]
async fn caller_guard_prepare_child() -> Result<()> {
    let Ok(server) = std::env::var("OAUTH_SYNTHETIC_SERVER") else {
        return Ok(());
    };
    let server: crate::server_config::ConfiguredServer = serde_json::from_str(&server)?;
    assert!(
        !server
            .definition
            .env
            .contains_key("MCP_POOL_CREDENTIALS_READ_ONLY"),
        "guard must come from the calling process, not the remote config env"
    );
    let definition = super::prepare(&server, false).await?;
    let authentication = definition
        .auth
        .ok_or_else(|| anyhow::anyhow!("prepared auth missing"))?;
    assert!(authentication.read_only);
    let path = std::env::var("OAUTH_SYNTHETIC_REQUEST_PATH")?;
    files::write_json(
        std::path::Path::new(&path),
        &serde_json::to_value(authentication)?,
    )?;
    Ok(())
}
