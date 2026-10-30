use anyhow::{Context, Result};
use serde_json::{Value, json};

#[path = "config_import_fixtures/auth.rs"]
mod auth;
#[path = "mcp_cli_fixtures/http.rs"]
mod http;
#[path = "config_import_fixtures/support.rs"]
mod support;
use auth::{credentials, destination, only_entry, vault};
use support::Fixture;

#[tokio::test]
async fn auth_copy_is_opt_in_private_redacted_and_idempotent() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join(".mcporter/mcporter.json");
    let cache = fixture.directory.join(".mcporter/credentials.json");
    let url = "https://example.invalid/mcp";
    fixture.write(
        &source,
        &json!({"mcpServers":{"remote":{"baseUrl":url,"auth":"oauth"}}}),
    )?;
    fixture.write(
        &cache,
        &vault(
            "remote",
            url,
            credentials("remote", url, "synthetic-access"),
        ),
    )?;
    let original = std::fs::read(&cache)?;
    fixture.success(&["import", "mcporter"]).await?;
    assert!(!destination(&fixture)?.exists());
    let preview = fixture
        .success(&[
            "import",
            "mcporter",
            "--include-auth",
            "--dry-run",
            "--json",
        ])
        .await?;
    let report: Value = serde_json::from_slice(&preview.stdout)?;
    assert_eq!(report.pointer("/servers/0/auth"), Some(&json!("import")));
    assert!(!destination(&fixture)?.exists());
    assert!(!String::from_utf8_lossy(&preview.stdout).contains("synthetic-access"));
    let output = fixture
        .success(&["config", "import", "mcporter", "--include-auth", "--json"])
        .await?;
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-access"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-refresh"));
    let saved = std::fs::read(destination(&fixture)?)?;
    let document: Value = serde_json::from_slice(&saved)?;
    let entry = only_entry(&document)?;
    assert_eq!(
        entry.pointer("/tokens/access_token"),
        Some(&json!("synthetic-access"))
    );
    assert_eq!(
        entry.pointer("/clientInfo/client_id"),
        Some(&json!("synthetic-client"))
    );
    assert!(entry.get("state").is_none_or(Value::is_null));
    assert!(entry.get("codeVerifier").is_none_or(Value::is_null));
    let repeated = fixture
        .success(&["import", "mcporter", "--include-auth", "--json"])
        .await?;
    let repeated: Value = serde_json::from_slice(&repeated.stdout)?;
    assert_eq!(
        repeated.pointer("/servers/0/auth"),
        Some(&json!("unchanged"))
    );
    assert_eq!(std::fs::read(destination(&fixture)?)?, saved);
    assert_eq!(std::fs::read(&cache)?, original);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(destination(&fixture)?)?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    fixture.finish().await
}

#[tokio::test]
async fn explicit_directory_auth_is_copied_not_shared_and_used_by_pooled_calls() -> Result<()> {
    let fixture = Fixture::new()?;
    let remote = http::HttpFixture::new().await?;
    let source = fixture.directory.join("selected.json");
    let cache = fixture.directory.join("tokens");
    fixture.write(&source, &json!({"mcpServers":{"remote":{"baseUrl":remote.url,"auth":"oauth","tokenCacheDir":"tokens"}}}))?;
    let payload = credentials("remote", &remote.url, "synthetic-directory-access");
    fixture.write(
        &cache.join("tokens.json"),
        payload.get("tokens").context("tokens")?,
    )?;
    fixture.write(
        &cache.join("client.json"),
        payload.get("clientInfo").context("client")?,
    )?;
    std::fs::write(cache.join("server_url.txt"), &remote.url)?;
    let original = std::fs::read(cache.join("tokens.json"))?;
    fixture
        .success(&[
            "import",
            "mcporter",
            "--from",
            source.to_str().context("source")?,
            "--include-auth",
        ])
        .await?;
    let config: Value = serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
    assert!(config.pointer("/mcpServers/remote/tokenCacheDir").is_none());
    let output = fixture
        .success(&["call", "remote.headers", "--output", "json", "--no-oauth"])
        .await?;
    let output: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        output.pointer("/headers/authorization"),
        Some(&json!("Bearer synthetic-directory-access"))
    );
    assert_eq!(std::fs::read(cache.join("tokens.json"))?, original);
    assert!(!cache.join("discovery.json").exists());
    let replacement = credentials("remote", &remote.url, "synthetic-replacement-access");
    fixture.write(
        &cache.join("tokens.json"),
        replacement.get("tokens").context("tokens")?,
    )?;
    fixture
        .success(&[
            "import",
            "mcporter",
            "--from",
            source.to_str().context("source")?,
            "--include-auth",
            "--replace",
        ])
        .await?;
    let output = fixture
        .success(&["call", "remote.headers", "--output", "json", "--no-oauth"])
        .await?;
    let output: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        output.pointer("/headers/authorization"),
        Some(&json!("Bearer synthetic-replacement-access"))
    );
    assert_eq!(
        remote
            .initializations
            .load(std::sync::atomic::Ordering::SeqCst),
        2
    );
    assert!(!remote.headers.lock().await.is_empty());
    fixture.finish().await
}

#[tokio::test]
async fn malformed_auth_or_conflicting_credentials_leave_config_and_vault_unchanged() -> Result<()>
{
    let fixture = Fixture::new()?;
    let source = fixture.directory.join(".mcporter/mcporter.json");
    let cache = fixture.directory.join(".mcporter/credentials.json");
    let url = "https://example.invalid/mcp";
    fixture.write(&source, &json!({"mcpServers":{"remote":{"baseUrl":url}}}))?;
    fixture.write(
        &cache,
        &vault("remote", url, credentials("remote", url, "first-access")),
    )?;
    fixture
        .success(&["import", "mcporter", "--include-auth"])
        .await?;
    let config = std::fs::read(fixture.home_config())?;
    let saved = std::fs::read(destination(&fixture)?)?;
    fixture.write(
        &cache,
        &vault("remote", url, credentials("remote", url, "second-access")),
    )?;
    let denied = fixture
        .run(&["import", "mcporter", "--include-auth"])
        .await?;
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--replace"));
    assert_eq!(std::fs::read(fixture.home_config())?, config);
    assert_eq!(std::fs::read(destination(&fixture)?)?, saved);
    fixture
        .success(&["import", "mcporter", "--include-auth", "--replace"])
        .await?;
    let saved: Value = serde_json::from_slice(&std::fs::read(destination(&fixture)?)?)?;
    assert_eq!(
        only_entry(&saved)?.pointer("/tokens/access_token"),
        Some(&json!("second-access"))
    );
    let saved = std::fs::read(destination(&fixture)?)?;
    std::fs::write(&cache, r#"{"tokens":"synthetic-secret-invalid-json""#)?;
    let denied = fixture
        .run(&["import", "mcporter", "--include-auth", "--replace"])
        .await?;
    assert!(!denied.status.success());
    assert!(!String::from_utf8_lossy(&denied.stderr).contains("synthetic-secret-invalid-json"));
    assert_eq!(std::fs::read(fixture.home_config())?, config);
    assert_eq!(std::fs::read(destination(&fixture)?)?, saved);
    fixture.success(&["import", "mcporter"]).await?;
    fixture.finish().await
}

#[tokio::test]
async fn auth_import_respects_selection_scope_and_read_only_policy() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join(".mcporter/mcporter.json");
    let cache = fixture.directory.join(".mcporter/credentials.json");
    let url = "https://example.invalid/mcp";
    fixture.write(
        &source,
        &json!({"mcpServers":{
            "remote":{"baseUrl":url},"other":{"baseUrl":"https://other.invalid/mcp"}
        }}),
    )?;
    fixture.write(
        &cache,
        &vault(
            "remote",
            url,
            credentials("remote", url, "synthetic-access"),
        ),
    )?;
    let denied = fixture
        .command(&fixture.project)
        .env("MCP_POOL_CREDENTIALS_READ_ONLY", "1")
        .args(["import", "mcporter", "remote", "--include-auth"])
        .output()
        .await?;
    assert!(!denied.status.success());
    assert!(!fixture.home_config().exists());
    assert!(!destination(&fixture)?.exists());
    fixture
        .success(&[
            "import",
            "mcporter",
            "remote",
            "--include-auth",
            "--scope",
            "project",
        ])
        .await?;
    assert!(!fixture.home_config().exists());
    let project_credentials = fixture.project.join(".mcp/credentials.json");
    let saved: Value = serde_json::from_slice(&std::fs::read(project_credentials)?)?;
    assert_eq!(
        only_entry(&saved)?.get("serverName"),
        Some(&json!("remote"))
    );
    fixture.finish().await
}

#[tokio::test]
async fn legacy_cache_expiration_is_not_extended_and_missing_auth_is_reported() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join(".mcporter/mcporter.json");
    let cache = fixture.directory.join(".mcporter/remote");
    fixture.write(
        &source,
        &json!({"mcpServers":{
            "remote":{"baseUrl":"https://example.invalid/mcp"},
            "missing":{"baseUrl":"https://missing.invalid/mcp"},
            "local":{"command":"never-execute-import-fixture"}
        }}),
    )?;
    fixture.write(
        &cache.join("tokens.json"),
        &json!({"access_token":"synthetic-expired","token_type":"Bearer","expires_in":3600}),
    )?;
    let output = fixture
        .success(&["import", "mcporter", "--include-auth", "--json"])
        .await?;
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        report.pointer("/servers/0/auth"),
        Some(&json!("unsupported"))
    );
    assert_eq!(report.pointer("/servers/1/auth"), Some(&json!("missing")));
    let saved: Value = serde_json::from_slice(&std::fs::read(destination(&fixture)?)?)?;
    assert!(only_entry(&saved)?.pointer("/tokens/expires_at").is_none());
    let doctor = fixture.success(&["config", "doctor", "--json"]).await?;
    assert!(!String::from_utf8_lossy(&doctor.stdout).contains("synthetic-expired"));
    fixture.finish().await
}

#[tokio::test]
async fn include_auth_rejects_other_sources_without_writes() -> Result<()> {
    let fixture = Fixture::new()?;
    let output = fixture.run(&["import", "claude", "--include-auth"]).await?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("mcporter"));
    assert!(!fixture.home_config().exists());
    fixture.finish().await
}

#[tokio::test]
async fn invalid_client_or_issuer_bindings_fail_before_writing_either_destination() -> Result<()> {
    for invalid in [
        "missing-client",
        "wrong-client",
        "wrong-issuer",
        "divergent-caches",
    ] {
        let fixture = Fixture::new()?;
        let url = "https://example.invalid/mcp";
        let source = fixture.directory.join(".mcporter/mcporter.json");
        let cache = fixture.directory.join(".mcporter/credentials.json");
        fixture.write(
            &source,
            &json!({"mcpServers":{"remote":{"baseUrl":url,"oauthClientId":"synthetic-client"}}}),
        )?;
        let mut entry = credentials("remote", url, "synthetic-access");
        match invalid {
            "missing-client" => {
                entry.as_object_mut().context("entry")?.remove("clientInfo");
            }
            "wrong-client" => {
                entry
                    .get_mut("clientInfo")
                    .and_then(Value::as_object_mut)
                    .context("client")?
                    .insert("client_id".to_owned(), json!("wrong-client"));
            }
            "wrong-issuer" => {
                entry
                    .get_mut("tokens")
                    .and_then(Value::as_object_mut)
                    .context("tokens")?
                    .insert("issuer".to_owned(), json!("https://issuer.invalid"));
                entry
                    .get_mut("clientInfo")
                    .and_then(Value::as_object_mut)
                    .context("client")?
                    .insert("issuer".to_owned(), json!("https://other.invalid"));
            }
            _ => {
                fixture.write(&fixture.directory.join(".mcporter/remote/tokens.json"), &json!({"access_token":"other-access","token_type":"Bearer","refresh_token":"divergent-refresh"}))?;
            }
        }
        fixture.write(&cache, &vault("remote", url, entry))?;
        let output = fixture
            .run(&["import", "mcporter", "--include-auth", "--replace"])
            .await?;
        assert!(!output.status.success(), "{invalid}");
        assert!(!fixture.home_config().exists(), "{invalid}");
        assert!(!destination(&fixture)?.exists(), "{invalid}");
        assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-access"));
        fixture.finish().await?;
    }
    Ok(())
}

#[tokio::test]
async fn xdg_vault_aliases_and_explicit_destinations_are_supported() -> Result<()> {
    let fixture = Fixture::new()?;
    let url = "https://example.invalid/mcp";
    let xdg = fixture.directory.join("data");
    let source = fixture.directory.join(".mcporter/mcporter.json");
    let selected = fixture.directory.join("isolated/config.json");
    fixture.write(&source, &json!({"mcpServers":{"remote":{"baseUrl":url,"headers":{"X-Header":"${UNSET_IMPORT_AUTH_HEADER}"},
        "env":{"XDG_DATA_HOME":xdg}}}}))?;
    fixture.write(
        &xdg.join("mcporter/credentials.json"),
        &json!({"version":1,"entries":{
            "remote-oauth|legacy-key":credentials("remote-oauth", url, "synthetic-alias-access"),
            "unrelated|other-key":credentials("unrelated", url, "must-not-copy")
        }}),
    )?;
    let output = fixture
        .success(&[
            "--config",
            selected.to_str().context("destination")?,
            "import",
            "mcporter",
            "--include-auth",
            "--dry-run",
            "--json",
        ])
        .await?;
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report.pointer("/servers/0/auth"), Some(&json!("import")));
    assert!(!selected.parent().context("parent")?.exists());
    fixture
        .success(&[
            "--config",
            selected.to_str().context("destination")?,
            "import",
            "mcporter",
            "--include-auth",
        ])
        .await?;
    let saved: Value =
        serde_json::from_slice(&std::fs::read(selected.with_file_name("credentials.json"))?)?;
    assert_eq!(
        only_entry(&saved)?.pointer("/tokens/access_token"),
        Some(&json!("synthetic-alias-access"))
    );
    assert!(!destination(&fixture)?.exists());
    fixture.finish().await
}
