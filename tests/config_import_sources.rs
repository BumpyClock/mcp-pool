use anyhow::{Context, Result};
use serde_json::{Value, json};

#[path = "config_import_fixtures/support.rs"]
mod support;
use support::Fixture;

fn imported(fixture: &Fixture) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(
        fixture.home_config(),
    )?)?)
}

#[tokio::test]
async fn cursor_project_wins_over_home_and_preserves_credentials_and_paths() -> Result<()> {
    let fixture = Fixture::new()?;
    #[cfg(windows)]
    let user_config = fixture
        .directory
        .join("Cursor")
        .join("User")
        .join("mcp.json");
    #[cfg(not(windows))]
    let user_config = fixture.directory.join(".cursor").join("mcp.json");
    fixture.write(
        &user_config,
        &json!({"mcpServers":{
            "shared":{"command":"home-command"},
            "home":{"url":"https://example.invalid/home","headers":{"X-Home":"${HOME_TOKEN}"}}
        }}),
    )?;
    fixture.write(
        &fixture.project.join(".cursor").join("mcp.json"),
        &json!({"mcpServers":{
            "shared":{"command":"./bin/server","args":["${ARG}"],"cwd":"workers","env":{"KEY":"${KEY}"}},
            "remote":{"url":"https://example.invalid/mcp","bearerTokenEnv":"TOKEN","auth":"oauth",
                "headers":{"X-Trace":"$env:TRACE"},"tokenCacheDir":"tokens"}
        }}),
    )?;
    fixture
        .success(&["import", "cursor", "shared", "home", "remote"])
        .await?;
    let document = imported(&fixture)?;
    assert_eq!(
        document.pointer("/mcpServers/shared/command"),
        Some(&json!([fixture.project.join("./bin/server"), "${ARG}"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/shared/cwd"),
        Some(&json!(fixture.project.join("workers")))
    );
    assert_eq!(
        document.pointer("/mcpServers/shared/env/KEY"),
        Some(&json!("${KEY}"))
    );
    assert_eq!(
        document.pointer("/mcpServers/home/headers/X-Home"),
        Some(&json!("${HOME_TOKEN}"))
    );
    assert_eq!(
        document.pointer("/mcpServers/remote/bearerTokenEnv"),
        Some(&json!("TOKEN"))
    );
    assert_eq!(
        document.pointer("/mcpServers/remote/auth"),
        Some(&json!("oauth"))
    );
    assert_eq!(
        document.pointer("/mcpServers/remote/headers/X-Trace"),
        Some(&json!("$env:TRACE"))
    );
    assert_eq!(
        document.pointer("/mcpServers/remote/tokenCacheDir"),
        Some(&json!(fixture.project.join(".cursor").join("tokens")))
    );
    fixture.finish().await
}

#[tokio::test]
async fn windsurf_discovers_user_config_and_preserves_literal_headers() -> Result<()> {
    let fixture = Fixture::new()?;
    #[cfg(windows)]
    let user_config = fixture
        .directory
        .join("Codeium")
        .join("windsurf")
        .join("mcp_config.json");
    #[cfg(not(windows))]
    let user_config = fixture
        .directory
        .join(".codeium")
        .join("windsurf")
        .join("mcp_config.json");
    fixture.write(
        &user_config,
        &json!({"mcpServers":{
            "remote":{"type":"sse","url":"https://example.invalid/events",
                "headers":{"Authorization":"$env:WINDSURF_AUTH"}},
            "first":{"command":"first-command"},"second":{"command":"extra"}
        }}),
    )?;
    fixture
        .success(&["import", "windsurf", "remote", "first", "second"])
        .await?;
    let document = imported(&fixture)?;
    assert_eq!(
        document.pointer("/mcpServers/remote/headers/Authorization"),
        Some(&json!("$env:WINDSURF_AUTH"))
    );
    assert_eq!(
        document.pointer("/mcpServers/remote/transport"),
        Some(&json!("sse"))
    );
    assert_eq!(
        document.pointer("/mcpServers/first/command"),
        Some(&json!(["first-command"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/second/command"),
        Some(&json!(["extra"]))
    );
    fixture.finish().await
}

#[tokio::test]
async fn opencode_jsonc_normalizes_local_remote_and_prefers_project() -> Result<()> {
    let fixture = Fixture::new()?;
    let home_file = fixture
        .directory
        .join(".config")
        .join("opencode")
        .join("opencode.jsonc");
    std::fs::create_dir_all(home_file.parent().context("parent")?)?;
    std::fs::write(
        &home_file,
        r#"{"mcp":{"shared":{"type":"local","command":["home"]},
           "home":{"type":"remote","url":"https://example.invalid/home","headers":{"X-Key":"{env:HOME_KEY}"}}}}"#,
    )?;
    let project_file = fixture.project.join("opencode.jsonc");
    std::fs::write(
        &project_file,
        r#"{ // project servers
            "mcp": {
                "shared": {"type":"local","command":["./tools/server","--flag"],
                           "environment":{"TOKEN":"{env:PROJECT_TOKEN}","PORT":9000}},
                "remote": {"type":"remote","url":"https://example.invalid/api",
                           "headers":{"Authorization":"$env:OPENCODE_AUTH"}},
            }
        }"#,
    )?;
    let mut command = fixture.command(&fixture.project);
    command.env("XDG_CONFIG_HOME", fixture.directory.join(".config"));
    let output = command
        .env_remove("OPENCODE_CONFIG")
        .env_remove("OPENCODE_CONFIG_DIR")
        .env_remove("OPENAI_WORKDIR")
        .args(["import", "opencode", "shared", "home", "remote"])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "OpenCode import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = imported(&fixture)?;
    assert_eq!(
        document.pointer("/mcpServers/shared/command"),
        Some(&json!([fixture.project.join("./tools/server"), "--flag"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/shared/env/TOKEN"),
        Some(&json!("$env:PROJECT_TOKEN"))
    );
    assert_eq!(
        document.pointer("/mcpServers/shared/env/PORT"),
        Some(&json!("9000"))
    );
    assert_eq!(
        document.pointer("/mcpServers/home/headers/X-Key"),
        Some(&json!("$env:HOME_KEY"))
    );
    assert_eq!(
        document.pointer("/mcpServers/remote/headers/Authorization"),
        Some(&json!("$env:OPENCODE_AUTH"))
    );
    fixture.finish().await
}

#[tokio::test]
async fn opencode_explicit_override_beats_project_jsonc_and_json() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.write(
        &fixture.project.join("opencode.json"),
        &json!({"mcp":{"shared":{"type":"local","command":["json"]},"json":{"command":["json-only"]}}}),
    )?;
    std::fs::write(
        fixture.project.join("opencode.jsonc"),
        r#"{"mcp":{"shared":{"type":"local","command":["jsonc"]}}}"#,
    )?;
    let override_path = fixture.directory.join("override.jsonc");
    let original = r#"{ // selected explicitly
       "mcp": {"shared": {"type":"local","command":["override"]}},
    }"#;
    std::fs::write(&override_path, original)?;
    let output = fixture
        .command(&fixture.project)
        .env("OPENCODE_CONFIG", &override_path)
        .env_remove("OPENCODE_CONFIG_DIR")
        .args(["import", "opencode", "shared", "json"])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "OpenCode override failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = imported(&fixture)?;
    assert_eq!(
        document.pointer("/mcpServers/shared/command"),
        Some(&json!(["override"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/json/command"),
        Some(&json!(["json-only"]))
    );
    assert_eq!(std::fs::read_to_string(override_path)?, original);
    fixture.finish().await
}

#[tokio::test]
async fn vscode_servers_and_aliases_support_project_priority_and_explicit_files() -> Result<()> {
    let fixture = Fixture::new()?;
    #[cfg(windows)]
    let user_config = fixture.directory.join("Code").join("User").join("mcp.json");
    #[cfg(target_os = "macos")]
    let user_config = fixture
        .directory
        .join("Library")
        .join("Application Support")
        .join("Code")
        .join("User")
        .join("mcp.json");
    #[cfg(not(any(windows, target_os = "macos")))]
    let user_config = fixture
        .directory
        .join(".config")
        .join("Code")
        .join("User")
        .join("mcp.json");
    fixture.write(
        &user_config,
        &json!({"servers":{"shared":{"command":"home-command"},"home":{"command":"home-only"}}}),
    )?;
    fixture.write(
        &fixture.project.join(".vscode").join("mcp.json"),
        &json!({"servers":{"shared":{"type":"stdio","command":"node","args":["server.js"]},
            "remote":{"type":"http","url":"https://example.invalid/api","headers":{"X-Auth":"${TOKEN}"}}}}),
    )?;
    let mut command = fixture.command(&fixture.project);
    command.env("XDG_CONFIG_HOME", fixture.directory.join(".config"));
    let output = command
        .args(["import", "vs-code", "shared", "home", "remote"])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "VS Code import failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document = imported(&fixture)?;
    assert_eq!(
        document.pointer("/mcpServers/shared/command"),
        Some(&json!(["node", "server.js"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/home/command"),
        Some(&json!(["home-only"]))
    );
    assert_eq!(
        document.pointer("/mcpServers/remote/headers/X-Auth"),
        Some(&json!("${TOKEN}"))
    );
    let explicit = fixture.directory.join("external").join("vscode.json");
    fixture.write(
        &explicit,
        &json!({"servers":{"explicit":{"command":"./server","cwd":"runtime"}}}),
    )?;
    fixture
        .success(&[
            "import",
            "vscode",
            "--from",
            explicit.to_str().context("path")?,
        ])
        .await?;
    let document = imported(&fixture)?;
    assert_eq!(
        document.pointer("/mcpServers/explicit/command"),
        Some(&json!([fixture
            .directory
            .join("external")
            .join("./server")]))
    );
    assert_eq!(
        document.pointer("/mcpServers/explicit/cwd"),
        Some(&json!(fixture.directory.join("external").join("runtime")))
    );
    fixture.finish().await
}

#[tokio::test]
async fn all_editor_sources_accept_explicit_files_without_modifying_them() -> Result<()> {
    let fixture = Fixture::new()?;
    for (kind, container) in [
        ("cursor", "mcpServers"),
        ("windsurf", "mcpServers"),
        ("opencode", "mcp"),
        ("vscode", "servers"),
    ] {
        let source = fixture.directory.join(format!("{kind}.jsonc"));
        let mut document = serde_json::Map::new();
        let mut entries = serde_json::Map::new();
        entries.insert(
            kind.into(),
            json!({"command":["echo"],"env":{"KEY":"${KEEP_REFERENCE}"}}),
        );
        document.insert(container.into(), Value::Object(entries));
        fixture.write(&source, &Value::Object(document))?;
        let original = std::fs::read(&source)?;
        fixture
            .success(&[
                "config",
                "import",
                kind,
                "--from",
                source.to_str().context("source path")?,
                kind,
            ])
            .await?;
        assert_eq!(std::fs::read(&source)?, original);
        let config = imported(&fixture)?;
        assert_eq!(
            config.pointer(&format!("/mcpServers/{kind}/env/KEY")),
            Some(&json!("${KEEP_REFERENCE}"))
        );
        assert_eq!(
            config.pointer(&format!("/mcpServers/{kind}/cwd")),
            Some(&json!(fixture.directory))
        );
    }
    fixture.finish().await
}

#[tokio::test]
async fn unsupported_helpers_and_transports_fail_before_mutation() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("source.json");
    for (kind, entry, reason) in [
        (
            "cursor",
            json!({"command":"server","headersHelper":"helper"}),
            "headersHelper",
        ),
        (
            "cursor",
            json!({"url":"https://example.invalid","bearer_token_env_var":"TOKEN"}),
            "bearer_token_env_var",
        ),
        (
            "windsurf",
            json!({"type":"websocket","url":"wss://example.invalid"}),
            "transport",
        ),
        (
            "windsurf",
            json!({"url":"https://example.invalid","refresh":{"accessTokenEnv":"TOKEN"}}),
            "refresh",
        ),
        (
            "opencode",
            json!({"type":"remote","url":"https://example.invalid","oauth":{"clientId":"secret"}}),
            "oauth",
        ),
        (
            "opencode",
            json!({"type":"local","command":["server"],"environment":{"KEY":"{file:/tmp/key}"}}),
            "file",
        ),
        (
            "vscode",
            json!({"command":"server","env":{"KEY":"${input:secret}"}}),
            "input",
        ),
    ] {
        let container = match kind {
            "opencode" => "mcp",
            "vscode" => "servers",
            _ => "mcpServers",
        };
        let mut document = serde_json::Map::new();
        document.insert(container.into(), json!({"fixture":entry}));
        fixture.write(&source, &Value::Object(document))?;
        let output = fixture
            .run(&["import", kind, "--from", source.to_str().context("path")?])
            .await?;
        assert!(
            !output.status.success(),
            "{kind} accepted unsupported input"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(reason),
            "{kind}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.home_config().exists());
    }
    fixture.finish().await
}

#[tokio::test]
async fn cursor_import_preserves_supported_oauth_metadata_aliases() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("oauth.json");
    fixture.write(
        &source,
        &json!({"mcpServers":{"oauth-server":{
            "url":"https://example.invalid/mcp",
            "auth":"oauth",
            "oauthTokenEndpointAuthMethod":"client_secret_post",
            "oauth_redirect_url":"http://127.0.0.1/callback",
            "oauth_requested_scope":"mcp.read",
            "oauthClientMetadataUrl":"https://example.invalid/client-metadata"
        }}}),
    )?;
    fixture
        .success(&[
            "import",
            "cursor",
            "--from",
            source.to_str().context("source path")?,
        ])
        .await?;
    let document = imported(&fixture)?;
    let server = document
        .pointer("/mcpServers/oauth-server")
        .context("imported OAuth server")?;
    assert_eq!(
        server.get("oauthTokenEndpointAuthMethod"),
        Some(&json!("client_secret_post"))
    );
    assert_eq!(
        server.get("oauth_redirect_url"),
        Some(&json!("http://127.0.0.1/callback"))
    );
    assert_eq!(
        server.get("oauth_requested_scope"),
        Some(&json!("mcp.read"))
    );
    assert_eq!(
        server.get("oauthClientMetadataUrl"),
        Some(&json!("https://example.invalid/client-metadata"))
    );
    fixture.finish().await
}
