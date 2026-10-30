use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

#[path = "config_import_fixtures/auth.rs"]
mod auth;
#[path = "config_import_fixtures/support.rs"]
mod support;
use auth::{credentials, destination, only_entry, vault};
use support::Fixture;

const URL: &str = "https://example.invalid/mcp";

#[tokio::test]
async fn source_runtime_home_selects_the_configured_account() -> Result<()> {
    for key in ["HOME", "USERPROFILE"] {
        let fixture = Fixture::new()?;
        let source = fixture.directory.join("selected.json");
        let home = fixture.directory.join("isolated-home");
        let selected = fixture.directory.join("selected-destination/config.json");
        fixture.write(
            &source,
            &json!({"mcpServers":{"remote":{"baseUrl":URL,"env":{key:home}}}}),
        )?;
        fixture.write(
            &home.join(".mcporter/credentials.json"),
            &vault(
                "remote",
                URL,
                credentials("remote", URL, "synthetic-configured-account"),
            ),
        )?;
        fixture.write(
            &fixture.directory.join(".mcporter/credentials.json"),
            &vault(
                "remote",
                URL,
                credentials("remote", URL, "synthetic-wrong-account"),
            ),
        )?;
        let mut command = fixture.command(&fixture.project);
        if key == "USERPROFILE" {
            command.env_remove("HOME");
        }
        let result = command
            .args([
                "--config",
                selected.to_str().context("destination")?,
                "import",
                "mcporter",
                "--from",
                source.to_str().context("source")?,
                "--include-auth",
            ])
            .output()
            .await?;
        ensure!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let saved: Value =
            serde_json::from_slice(&std::fs::read(selected.with_file_name("credentials.json"))?)?;
        assert_eq!(
            only_entry(&saved)?.pointer("/tokens/access_token"),
            Some(&json!("synthetic-configured-account"))
        );
        fixture.finish().await?;
    }
    Ok(())
}

#[tokio::test]
async fn source_cache_path_references_resolve_without_expanding_unrelated_secrets() -> Result<()> {
    for reference in ["$env:SYNTHETIC_CACHE_PATH", "${SYNTHETIC_CACHE_PATH}"] {
        let fixture = Fixture::new()?;
        let source = fixture.directory.join("selected.json");
        let cache = fixture.directory.join("configured-cache");
        fixture.write(&source, &json!({"mcpServers":{"remote":{
            "baseUrl":URL,"tokenCacheDir":reference,"headers":{"X-Secret":"${MISSING_UNRELATED_SECRET}"},
            "env":{"SYNTHETIC_CACHE_PATH":cache,"UNRELATED":"${MISSING_UNRELATED_SECRET}"}
        }}}))?;
        let entry = credentials("remote", URL, "synthetic-path-access");
        fixture.write(
            &cache.join("tokens.json"),
            entry.get("tokens").context("tokens")?,
        )?;
        fixture.write(
            &cache.join("client.json"),
            entry.get("clientInfo").context("client")?,
        )?;
        let result = fixture
            .command(&fixture.project)
            .env(
                "SYNTHETIC_CACHE_PATH",
                fixture.directory.join("wrong-cache"),
            )
            .args([
                "import",
                "mcporter",
                "--from",
                source.to_str().context("source")?,
                "--include-auth",
            ])
            .output()
            .await?;
        ensure!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let saved: Value = serde_json::from_slice(&std::fs::read(destination(&fixture)?)?)?;
        assert_eq!(
            only_entry(&saved)?.pointer("/tokens/access_token"),
            Some(&json!("synthetic-path-access"))
        );
        let config: Value = serde_json::from_slice(&std::fs::read(fixture.home_config())?)?;
        assert_eq!(
            config.pointer("/mcpServers/remote/env/UNRELATED"),
            Some(&json!("${MISSING_UNRELATED_SECRET}"))
        );
        assert!(config.pointer("/mcpServers/remote/tokenCacheDir").is_none());
        fixture.finish().await?;
    }
    Ok(())
}

#[tokio::test]
async fn shared_source_and_destination_vaults_are_rejected_before_config_changes() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join(".mcporter/mcporter.json");
    let cache = source.with_file_name("credentials.json");
    let selected = source.with_file_name("pool-config.json");
    fixture.write(&source, &json!({"mcpServers":{"remote":{"baseUrl":URL}}}))?;
    fixture.write(
        &cache,
        &vault(
            "remote",
            URL,
            credentials("remote", URL, "synthetic-source-access"),
        ),
    )?;
    let original = std::fs::read(&cache)?;
    let result = fixture
        .run(&[
            "--config",
            selected.to_str().context("destination")?,
            "import",
            "mcporter",
            "--include-auth",
            "--replace",
        ])
        .await?;
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("alias"));
    assert!(!selected.exists());
    assert_eq!(std::fs::read(&cache)?, original);
    fixture.finish().await
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_destination_vault_cannot_alias_the_source() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join(".mcporter/mcporter.json");
    let cache = source.with_file_name("credentials.json");
    fixture.write(&source, &json!({"mcpServers":{"remote":{"baseUrl":URL}}}))?;
    fixture.write(
        &cache,
        &vault(
            "remote",
            URL,
            credentials("remote", URL, "synthetic-source-access"),
        ),
    )?;
    let selected = fixture.directory.join("isolated/config.json");
    std::fs::create_dir_all(selected.parent().context("parent")?)?;
    std::os::unix::fs::symlink(&cache, selected.with_file_name("credentials.json"))?;
    let original = std::fs::read(&cache)?;
    let result = fixture
        .run(&[
            "--config",
            selected.to_str().context("destination")?,
            "import",
            "mcporter",
            "--include-auth",
        ])
        .await?;
    assert!(!result.status.success());
    assert!(!selected.exists());
    assert_eq!(std::fs::read(&cache)?, original);
    fixture.finish().await
}

#[tokio::test]
async fn registration_and_unfinished_authorization_require_explicit_replacement() -> Result<()> {
    for registration in [true, false] {
        let fixture = Fixture::new()?;
        let source = fixture.directory.join(".mcporter/mcporter.json");
        let cache = source.with_file_name("credentials.json");
        fixture.write(&source, &json!({"mcpServers":{"remote":{"baseUrl":URL}}}))?;
        fixture.write(
            &cache,
            &vault(
                "remote",
                URL,
                credentials("remote", URL, "synthetic-source-access"),
            ),
        )?;
        let mut existing = json!({"serverName":"remote","serverUrl":URL,"state":{"active":"pending-consent"},"codeVerifier":"pending-verifier"});
        if registration {
            existing.as_object_mut().context("entry")?.insert(
                "clientInfo".to_owned(),
                json!({"client_id":"existing-client","client_secret":"synthetic-existing-secret"}),
            );
        }
        fixture.write(&destination(&fixture)?, &vault("remote", URL, existing))?;
        let original = std::fs::read(destination(&fixture)?)?;
        let result = fixture
            .run(&["import", "mcporter", "--include-auth"])
            .await?;
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("--replace"));
        assert!(!fixture.home_config().exists());
        assert_eq!(std::fs::read(destination(&fixture)?)?, original);
        fixture
            .success(&["import", "mcporter", "--include-auth", "--replace"])
            .await?;
        let saved: Value = serde_json::from_slice(&std::fs::read(destination(&fixture)?)?)?;
        assert_eq!(
            only_entry(&saved)?.pointer("/clientInfo/client_id"),
            Some(&json!("synthetic-client"))
        );
        fixture.finish().await?;
    }
    Ok(())
}

#[tokio::test]
async fn replacement_validates_the_new_client_without_reusing_the_old_binding() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join(".mcporter/mcporter.json");
    let cache = source.with_file_name("credentials.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{"remote":{"baseUrl":URL,"oauthClientId":"synthetic-client"}}}),
    )?;
    fixture.write(
        &cache,
        &vault(
            "remote",
            URL,
            credentials("remote", URL, "synthetic-new-access"),
        ),
    )?;
    let mut existing = credentials("remote", URL, "synthetic-old-access");
    existing
        .as_object_mut()
        .context("entry")?
        .insert("clientInfo".to_owned(), json!({"client_id":"old-client"}));
    fixture.write(&destination(&fixture)?, &vault("remote", URL, existing))?;
    fixture.write(
        &fixture.home_config(),
        &json!({"mcpServers":{"remote":{"baseUrl":URL,"oauthClientId":"old-client"}}}),
    )?;
    fixture
        .success(&["import", "mcporter", "--include-auth", "--replace"])
        .await?;
    let saved: Value = serde_json::from_slice(&std::fs::read(destination(&fixture)?)?)?;
    assert_eq!(
        only_entry(&saved)?.pointer("/tokens/access_token"),
        Some(&json!("synthetic-new-access"))
    );
    assert_eq!(
        only_entry(&saved)?.pointer("/clientInfo/client_id"),
        Some(&json!("synthetic-client"))
    );
    fixture.finish().await
}

#[tokio::test]
async fn imported_endpoint_identity_matches_future_runtime_loading() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join(".mcporter/mcporter.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{"remote":{
            "baseUrl":"${SYNTHETIC_ENDPOINT}","env":{"SYNTHETIC_ENDPOINT":URL}
        }}}),
    )?;
    fixture.write(
        &source.with_file_name("credentials.json"),
        &vault(
            "remote",
            URL,
            credentials("remote", URL, "synthetic-configured-endpoint"),
        ),
    )?;
    for arguments in [
        vec!["import", "mcporter", "--include-auth"],
        vec!["config", "doctor", "--json"],
    ] {
        let result = fixture
            .command(&fixture.project)
            .env("SYNTHETIC_ENDPOINT", "https://wrong.invalid/mcp")
            .args(&arguments)
            .output()
            .await?;
        ensure!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        if arguments.first() == Some(&"config") {
            let report: Value = serde_json::from_slice(&result.stdout)?;
            assert_eq!(
                report.pointer("/servers/0/credentials/authenticated"),
                Some(&json!(true))
            );
        }
    }
    fixture.finish().await
}

#[tokio::test]
async fn identical_tokens_do_not_hide_a_different_destination_endpoint_binding() -> Result<()> {
    for marker_only in [true, false] {
        let fixture = Fixture::new()?;
        let source = fixture.directory.join(".mcporter/mcporter.json");
        let old_url = "https://old.invalid/mcp";
        let stored_url = if marker_only { URL } else { old_url };
        fixture.write(&source, &json!({"mcpServers":{"remote":{"baseUrl":URL}}}))?;
        let mut incoming = credentials("remote", URL, "synthetic-identical-access");
        incoming.as_object_mut().context("entry")?.remove("state");
        incoming
            .as_object_mut()
            .context("entry")?
            .remove("codeVerifier");
        fixture.write(
            &source.with_file_name("credentials.json"),
            &vault("remote", URL, incoming.clone()),
        )?;
        incoming
            .as_object_mut()
            .context("entry")?
            .insert("serverUrl".to_owned(), json!(stored_url));
        let mut previous = vault("remote", stored_url, incoming);
        previous
            .get_mut("serverUrls")
            .and_then(Value::as_object_mut)
            .context("bindings")?
            .insert("remote".to_owned(), json!(old_url));
        fixture.write(&destination(&fixture)?, &previous)?;
        fixture.write(
            &fixture.home_config(),
            &json!({"mcpServers":{"remote":{"baseUrl":stored_url}}}),
        )?;
        let original = std::fs::read(destination(&fixture)?)?;
        let denied = fixture
            .run(&["import", "mcporter", "--include-auth"])
            .await?;
        assert!(!denied.status.success());
        assert_eq!(std::fs::read(destination(&fixture)?)?, original);
        let output = fixture
            .success(&[
                "import",
                "mcporter",
                "--include-auth",
                "--replace",
                "--json",
            ])
            .await?;
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(report.pointer("/servers/0/auth"), Some(&json!("import")));
        let output = fixture.success(&["config", "doctor", "--json"]).await?;
        let report: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(
            report.pointer("/servers/0/credentials/authenticated"),
            Some(&json!(true))
        );
        fixture.finish().await?;
    }
    Ok(())
}

#[tokio::test]
async fn malformed_cached_discovery_and_unresolved_cache_paths_fail_before_writes() -> Result<()> {
    for cache_path in [None, Some("$env:MISSING_IMPORT_CACHE_DIRECTORY")] {
        let fixture = Fixture::new()?;
        let source = fixture.directory.join(".mcporter/mcporter.json");
        let mut definition = json!({"baseUrl":URL});
        if let Some(path) = cache_path {
            definition
                .as_object_mut()
                .context("definition")?
                .insert("tokenCacheDir".to_owned(), json!(path));
        }
        fixture.write(&source, &json!({"mcpServers":{"remote":definition}}))?;
        let mut entry = credentials("remote", URL, "synthetic-source-access");
        entry.as_object_mut().context("entry")?.insert("discoveryState".to_owned(), json!({"authorizationServerMetadata":{
            "issuer":"https://issuer.invalid","authorization_endpoint":"https://issuer.invalid/authorize","token_endpoint":"http://issuer.invalid/token"
        }}));
        fixture.write(
            &source.with_file_name("credentials.json"),
            &vault("remote", URL, entry),
        )?;
        let result = fixture
            .run(&["import", "mcporter", "--include-auth"])
            .await?;
        assert!(!result.status.success());
        assert!(!fixture.home_config().exists());
        assert!(!destination(&fixture)?.exists());
        fixture.finish().await?;
    }
    Ok(())
}
