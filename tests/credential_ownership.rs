#[path = "common/fixture_lifecycle.rs"]
mod fixture_lifecycle;

use std::path::Path;
use std::sync::atomic::Ordering;

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

#[path = "mcp_cli_fixtures/http.rs"]
mod http;
#[path = "config_import_fixtures/support.rs"]
mod support;
use support::Fixture;

async fn set_tokens(fixture: &Fixture, token: &str, expiration: u64) -> Result<()> {
    let tokens = fixture.directory.join("tokens.json");
    fixture.write(
        &tokens,
        &json!({"access_token":token,"token_type":"Bearer","expires_at":expiration}),
    )?;
    fixture
        .success(&[
            "vault",
            "set",
            "protected",
            "--tokens-file",
            tokens.to_str().context("tokens path")?,
        ])
        .await?;
    Ok(())
}

async fn authorization_header(fixture: &Fixture, directory: &Path) -> Result<String> {
    let output = fixture
        .command(directory)
        .args(["call", "protected.headers", "--output", "json"])
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "Call failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout)?;
    result
        .pointer("/headers/authorization")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("authorization header")
}

#[tokio::test]
async fn credentials_follow_the_winning_definition_across_projects_and_mutations() -> Result<()> {
    let fixture = Fixture::new()?;
    let remote = http::HttpFixture::new().await?;
    let second = fixture.directory.join("second");
    std::fs::create_dir(&second)?;
    let definition = json!({"mcpServers":{"protected":{"url":remote.url,"auth":"oauth"}}});
    fixture.write(&fixture.home_config(), &definition)?;
    let home_credentials = fixture.home_config().with_file_name("credentials.json");
    let project_config = fixture.project.join(".mcp/config.json");
    let project_credentials = project_config.with_file_name("credentials.json");

    set_tokens(&fixture, "home-fixture-token", 4102444800).await?;
    assert!(home_credentials.is_file());
    assert!(!project_credentials.exists());
    assert_eq!(
        authorization_header(&fixture, &fixture.project).await?,
        "Bearer home-fixture-token"
    );
    assert_eq!(
        authorization_header(&fixture, &second).await?,
        "Bearer home-fixture-token"
    );
    assert_eq!(remote.initializations.load(Ordering::SeqCst), 1);

    fixture.write(&project_config, &definition)?;
    let missing = fixture
        .run(&["call", "protected.headers", "--no-oauth"])
        .await?;
    assert!(
        !missing.status.success(),
        "Project definition borrowed home credentials"
    );
    set_tokens(&fixture, "project-fixture-token", 4102444800).await?;
    assert!(project_credentials.is_file());
    assert_eq!(
        authorization_header(&fixture, &fixture.project).await?,
        "Bearer project-fixture-token"
    );
    assert_eq!(
        authorization_header(&fixture, &second).await?,
        "Bearer home-fixture-token"
    );
    assert_eq!(remote.initializations.load(Ordering::SeqCst), 2);

    let listing: Value =
        serde_json::from_slice(&fixture.success(&["pool", "list", "--json"]).await?.stdout)?;
    let status: Value = serde_json::from_slice(
        &fixture
            .success(&["status", "protected", "--json"])
            .await?
            .stdout,
    )?;
    assert_eq!(
        listing.pointer("/0/socket_path"),
        status.pointer("/servers/0/socket_path")
    );
    let nested = fixture.project.join("nested");
    std::fs::create_dir(&nested)?;
    assert_eq!(
        authorization_header(&fixture, &nested).await?,
        "Bearer project-fixture-token"
    );

    let tokens = fixture.directory.join("tokens.json");
    fixture.write(&tokens, &json!({"access_token":"updated-home-fixture-token","token_type":"Bearer","expires_at":4102444800u64}))?;
    let updated = fixture
        .command(&second)
        .args([
            "vault",
            "set",
            "protected",
            "--tokens-file",
            tokens.to_str().context("tokens path")?,
        ])
        .output()
        .await?;
    ensure!(updated.status.success(), "Home credential update failed");
    assert_eq!(
        authorization_header(&fixture, &fixture.project).await?,
        "Bearer project-fixture-token"
    );
    assert_eq!(remote.initializations.load(Ordering::SeqCst), 2);
    assert_eq!(
        authorization_header(&fixture, &second).await?,
        "Bearer updated-home-fixture-token"
    );
    assert_eq!(remote.initializations.load(Ordering::SeqCst), 3);

    set_tokens(&fixture, "expired-project-token", 1).await?;
    assert!(
        !fixture
            .run(&["call", "protected.headers", "--no-oauth"])
            .await?
            .status
            .success()
    );
    fixture.success(&["config", "logout", "protected"]).await?;
    assert!(
        !fixture
            .run(&["call", "protected.headers", "--no-oauth"])
            .await?
            .status
            .success(),
        "Logout exposed home credentials through a project definition"
    );
    set_tokens(&fixture, "obsolete-project-token", 1).await?;
    std::fs::remove_file(&project_config)?;
    assert_eq!(
        authorization_header(&fixture, &fixture.project).await?,
        "Bearer updated-home-fixture-token"
    );
    assert_eq!(remote.initializations.load(Ordering::SeqCst), 3);
    assert!(!remote.headers.lock().await.is_empty());
    let home_before = std::fs::read(&home_credentials)?;
    fixture
        .success(&["auth", "protected", "--no-browser"])
        .await?;
    assert_eq!(std::fs::read(&home_credentials)?, home_before);
    fixture.success(&["vault", "clear", "protected"]).await?;
    assert!(
        !fixture
            .command(&second)
            .args(["call", "protected.headers", "--no-oauth"])
            .output()
            .await?
            .status
            .success()
    );
    fixture.finish().await
}

#[tokio::test]
async fn explicit_configuration_uses_its_own_store_for_reads_and_writes() -> Result<()> {
    let fixture = Fixture::new()?;
    let remote = http::HttpFixture::new().await?;
    let definition = json!({"mcpServers":{"protected":{"url":remote.url,"auth":"oauth"}}});
    fixture.write(&fixture.home_config(), &definition)?;
    set_tokens(&fixture, "home-fixture-token", 4102444800).await?;
    let explicit = fixture.directory.join("explicit/config.json");
    fixture.write(&explicit, &definition)?;
    let path = explicit.to_str().context("explicit path")?;
    let missing = fixture
        .run(&["--config", path, "call", "protected.headers", "--no-oauth"])
        .await?;
    assert!(
        !missing.status.success(),
        "Explicit configuration borrowed home credentials"
    );
    let tokens = fixture.directory.join("tokens.json");
    fixture.write(&tokens, &json!({"access_token":"explicit-fixture-token","token_type":"Bearer","expires_at":4102444800u64}))?;
    fixture
        .success(&[
            "--config",
            path,
            "vault",
            "set",
            "protected",
            "--tokens-file",
            tokens.to_str().context("tokens path")?,
        ])
        .await?;
    assert!(explicit.with_file_name("credentials.json").is_file());
    let output = fixture
        .success(&[
            "--config",
            path,
            "call",
            "protected.headers",
            "--output",
            "json",
        ])
        .await?;
    let result: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(
        result.pointer("/headers/authorization"),
        Some(&json!("Bearer explicit-fixture-token"))
    );
    assert_eq!(
        authorization_header(&fixture, &fixture.project).await?,
        "Bearer home-fixture-token"
    );
    fixture
        .success(&["--config", path, "vault", "clear", "protected"])
        .await?;
    assert!(
        !fixture
            .run(&["--config", path, "call", "protected.headers", "--no-oauth"])
            .await?
            .status
            .success()
    );
    assert_eq!(
        authorization_header(&fixture, &fixture.project).await?,
        "Bearer home-fixture-token"
    );
    fixture.finish().await
}
