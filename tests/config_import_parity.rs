use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

#[path = "config_import_fixtures/support.rs"]
mod support;
use support::Fixture;
#[path = "config_import_fixtures/parity/support.rs"]
mod parity;
use parity::{
    CANARY, cursor_user_path, no_canary, opencode_user_path, path_string, read_config, run, server,
    success, vscode_user_path, windsurf_user_path, write_source,
};

const CURSOR: &str = include_str!("config_import_fixtures/parity/cursor.jsonc");
const WINDSURF: &str = include_str!("config_import_fixtures/parity/windsurf.json");
const OPENCODE: &str = include_str!("config_import_fixtures/parity/opencode.jsonc");
const VSCODE: &str = include_str!("config_import_fixtures/parity/vscode.jsonc");
const CODEX: &str = include_str!("config_import_fixtures/parity/codex.toml");

#[tokio::test]
async fn cursor_jsonc_project_overrides_user_and_preserves_unresolved_references() -> Result<()> {
    let fixture = Fixture::new()?;
    let user = cursor_user_path(&fixture);
    let project = fixture.project.join(".cursor").join("mcp.json");
    fixture.write(
        &user,
        &json!({"mcpServers":{
            "shared":{"command":"user-server"},
            "user-only":{"command":"user-only"}
        }}),
    )?;
    write_source(&project, CURSOR)?;
    let source_before = std::fs::read(&project)?;
    let output = success(
        &fixture,
        &["import", "cursor", "--scope", "project", "--json"],
    )
    .await?;
    let destination = fixture.project.join(".mcp").join("config.json");
    let document = read_config(&destination)?;
    let shared = server(&document, "shared")?;
    let command = shared
        .pointer("/command/0")
        .and_then(Value::as_str)
        .context("Cursor executable")?;
    assert_eq!(
        Path::new(command).components().collect::<PathBuf>(),
        fixture.project.join("bin").join("project-server")
    );
    assert_eq!(shared.pointer("/command/1"), Some(&json!("--literal")));
    assert_eq!(
        shared.pointer("/command/2"),
        Some(&json!("${PARITY_UNSET_ARGUMENT}"))
    );
    assert_eq!(
        shared.get("cwd"),
        Some(&json!(fixture.project.join("workers")))
    );
    assert_eq!(
        shared.pointer("/env/PARITY_KEY"),
        Some(&json!("${PARITY_UNSET_ENV}"))
    );
    assert_eq!(
        server(&document, "user-only")?.get("command"),
        Some(&json!(["user-only"]))
    );
    let remote = server(&document, "oauth-docs")?;
    assert_eq!(remote.get("auth"), Some(&json!("oauth")));
    assert_eq!(
        remote.pointer("/headers/X-Client"),
        Some(&json!("${PARITY_UNSET_HEADER}"))
    );
    assert_eq!(
        remote.get("tokenCacheDir"),
        Some(&json!(
            project.parent().context("cursor parent")?.join("tokens")
        ))
    );
    assert_eq!(std::fs::read(&project)?, source_before);
    assert!(!fixture.home_config().exists());
    no_canary(&output, &document)?;
    fixture.finish().await
}

#[tokio::test]
async fn explicit_from_uses_only_that_cursor_file_not_discovered_files() -> Result<()> {
    let fixture = Fixture::new()?;
    write_source(&fixture.project.join(".cursor").join("mcp.json"), CURSOR)?;
    write_source(
        &cursor_user_path(&fixture),
        r#"{"mcpServers":{"discovered":{"command":"never-selected"}}}"#,
    )?;
    let chosen = fixture.directory.join("selected.jsonc");
    write_source(
        &chosen,
        "{ // independent explicit source\n\"mcpServers\":{\"chosen\":{\"command\":\"node\"}},\n}",
    )?;
    success(
        &fixture,
        &[
            "config",
            "import",
            "cursor",
            "--from",
            path_string(&chosen)?,
        ],
    )
    .await?;
    let document = read_config(&fixture.home_config())?;
    assert_eq!(
        server(&document, "chosen")?.get("command"),
        Some(&json!(["node"]))
    );
    assert!(document.pointer("/mcpServers/shared").is_none());
    assert!(document.pointer("/mcpServers/discovered").is_none());
    fixture.finish().await
}

#[tokio::test]
async fn windsurf_global_root_dictionary_and_codex_toml_are_explicit_copies() -> Result<()> {
    let fixture = Fixture::new()?;
    let windsurf = windsurf_user_path(&fixture);
    write_source(&windsurf, WINDSURF)?;
    write_source(&fixture.directory.join(".codex").join("config.toml"), CODEX)?;
    let windsurf_before = std::fs::read(&windsurf)?;
    success(&fixture, &["import", "windsurf"]).await?;
    success(&fixture, &["config", "import", "codex"]).await?;
    let document = read_config(&fixture.home_config())?;
    let wind = server(&document, "wind")?;
    assert_eq!(wind.get("command"), Some(&json!(["node", "wind.js"])));
    assert_eq!(
        wind.pointer("/env/WINDSURF_KEY"),
        Some(&json!("$env:PARITY_UNSET_WIND_KEY"))
    );
    let codex = server(&document, "codex")?;
    assert_eq!(
        codex.get("bearerTokenEnv"),
        Some(&json!("PARITY_UNSET_CODEX_BEARER"))
    );
    assert_eq!(
        codex.pointer("/headers/X-Codex"),
        Some(&json!("$env:PARITY_UNSET_CODEX_HEADER"))
    );
    assert_eq!(std::fs::read(&windsurf)?, windsurf_before);
    fixture.finish().await
}

#[tokio::test]
async fn opencode_jsonc_normalizes_local_and_remote_with_project_precedence() -> Result<()> {
    let fixture = Fixture::new()?;
    write_source(
        &opencode_user_path(&fixture),
        r#"{"mcp":{"remote":{"type":"remote","url":"https://example.invalid/user"},"global":{"type":"local","command":["node","global.js"]}}}"#,
    )?;
    write_source(&fixture.project.join("opencode.jsonc"), OPENCODE)?;
    success(&fixture, &["import", "opencode", "--scope", "project"]).await?;
    let document = read_config(&fixture.project.join(".mcp").join("config.json"))?;
    let local = server(&document, "local")?;
    let command = local
        .pointer("/command/0")
        .and_then(Value::as_str)
        .context("OpenCode executable")?;
    assert_eq!(
        Path::new(command).components().collect::<PathBuf>(),
        fixture.project.join("bin").join("opencode-server")
    );
    assert_eq!(
        local.pointer("/command/1"),
        Some(&json!("${PARITY_UNSET_ARG}"))
    );
    assert_eq!(
        local.pointer("/env/OPENCODE_KEY"),
        Some(&json!("${PARITY_UNSET_KEY}"))
    );
    assert_eq!(
        server(&document, "remote")?.get("url"),
        Some(&json!("https://example.invalid/opencode"))
    );
    assert_eq!(
        server(&document, "remote")?.pointer("/headers/X-OpenCode"),
        Some(&json!("${PARITY_UNSET_HEADER}"))
    );
    assert!(server(&document, "global").is_ok());
    fixture.finish().await
}

#[tokio::test]
async fn vscode_servers_container_project_overrides_user_and_rebases_local_commands() -> Result<()>
{
    let fixture = Fixture::new()?;
    write_source(
        &vscode_user_path(&fixture),
        r#"{"servers":{"editor":{"type":"http","url":"https://example.invalid/user"},"user-only":{"command":"node","args":["user.js"]}}}"#,
    )?;
    write_source(&fixture.project.join(".vscode").join("mcp.json"), VSCODE)?;
    success(&fixture, &["import", "vs-code", "--scope", "project"]).await?;
    let document = read_config(&fixture.project.join(".mcp").join("config.json"))?;
    success(
        &fixture,
        &["config", "import", "vscode", "--scope", "project"],
    )
    .await?;
    assert_eq!(
        read_config(&fixture.project.join(".mcp").join("config.json"))?,
        document
    );
    assert_eq!(
        server(&document, "editor")?.get("url"),
        Some(&json!("https://example.invalid/vscode"))
    );
    assert_eq!(
        server(&document, "editor")?.pointer("/headers/X-VSCode"),
        Some(&json!("${PARITY_UNSET_VSCODE_KEY}"))
    );
    assert_eq!(
        server(&document, "user-only")?.get("command"),
        Some(&json!(["node", "user.js"]))
    );
    fixture.finish().await
}

#[tokio::test]
async fn unsupported_auth_helpers_fail_without_partial_writes_or_private_output() -> Result<()> {
    let fixture = Fixture::new()?;
    let path = fixture.directory.join("helper.json");
    for (source, content) in [
        (
            "cursor",
            r#"{"mcpServers":{"safe":{"command":"node"},"private":{"command":"node","headersHelper":"parity-private-helper-value"}}}"#,
        ),
        (
            "windsurf",
            r#"{"mcpServers":{"safe":{"command":"node"},"private":{"url":"https://example.invalid","auth":"parity-private-helper-value"}}}"#,
        ),
        (
            "opencode",
            r#"{"mcp":{"safe":{"type":"local","command":["node"]},"private":{"type":"remote","url":"https://example.invalid","oauth":{"clientSecret":"parity-private-helper-value"}}}}"#,
        ),
    ] {
        write_source(&path, content)?;
        let output = run(&fixture, &["import", source, "--from", path_string(&path)?]).await?;
        assert!(
            !output.status.success(),
            "{source} accepted unsupported auth"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.to_lowercase().contains("unsupported"),
            "{source} did not explain unsupported authentication"
        );
        assert!(!stderr.contains("parity-private-helper-value"));
        assert!(!String::from_utf8_lossy(&output.stdout).contains("parity-private-helper-value"));
        assert!(!fixture.home_config().exists());
    }
    fixture.finish().await
}

#[tokio::test]
async fn dry_run_conflict_and_replace_keep_destination_atomic_and_sources_untouched() -> Result<()>
{
    let fixture = Fixture::new()?;
    let source = fixture.directory.join("batch.jsonc");
    write_source(
        &source,
        "{ // two entries, one conflicting\n\"servers\":{\"editor\":{\"type\":\"http\",\"url\":\"https://example.invalid/new\"},\"addition\":{\"command\":\"node\"}},\n}",
    )?;
    let original_source = std::fs::read(&source)?;
    let destination = fixture.home_config();
    let preview = success(
        &fixture,
        &[
            "import",
            "vscode",
            "--from",
            path_string(&source)?,
            "--dry-run",
            "--json",
        ],
    )
    .await?;
    assert_eq!(
        serde_json::from_slice::<Value>(&preview.stdout)?.get("dryRun"),
        Some(&json!(true))
    );
    assert!(!destination.exists());
    write_source(
        &destination,
        r#"{"mcpServers":{"editor":{"url":"https://example.invalid/original"},"kept":{"command":"existing"}},"custom":{"enabled":true}}"#,
    )?;
    let original_destination = std::fs::read(&destination)?;
    let denied = run(
        &fixture,
        &["import", "vscode", "--from", path_string(&source)?],
    )
    .await?;
    assert!(!denied.status.success());
    assert_eq!(std::fs::read(&destination)?, original_destination);
    let denied_preview = run(
        &fixture,
        &[
            "import",
            "vscode",
            "--from",
            path_string(&source)?,
            "--dry-run",
        ],
    )
    .await?;
    assert!(!denied_preview.status.success());
    assert_eq!(std::fs::read(&destination)?, original_destination);
    let replace_preview = success(
        &fixture,
        &[
            "import",
            "vscode",
            "--from",
            path_string(&source)?,
            "--replace",
            "--dry-run",
            "--json",
        ],
    )
    .await?;
    assert_eq!(
        serde_json::from_slice::<Value>(&replace_preview.stdout)?.get("dryRun"),
        Some(&json!(true))
    );
    assert_eq!(std::fs::read(&destination)?, original_destination);
    success(
        &fixture,
        &[
            "import",
            "vscode",
            "--from",
            path_string(&source)?,
            "--replace",
        ],
    )
    .await?;
    let imported = read_config(&destination)?;
    assert_eq!(
        server(&imported, "editor")?.get("url"),
        Some(&json!("https://example.invalid/new"))
    );
    assert!(server(&imported, "addition").is_ok());
    assert_eq!(
        server(&imported, "kept")?.get("command"),
        Some(&json!("existing"))
    );
    assert_eq!(imported.get("custom"), Some(&json!({"enabled":true})));
    success(
        &fixture,
        &["import", "vscode", "--from", path_string(&source)?],
    )
    .await?;
    assert_eq!(read_config(&destination)?, imported);
    assert_eq!(std::fs::read(&source)?, original_source);
    fixture.finish().await
}

#[tokio::test]
async fn copy_from_adds_alias_then_applies_overrides_without_requiring_new_endpoint() -> Result<()>
{
    let fixture = Fixture::new()?;
    let source = fixture.project.join(".cursor").join("mcp.json");
    write_source(&source, CURSOR)?;
    let original_source = std::fs::read(&source)?;
    success(
        &fixture,
        &[
            "config",
            "add",
            "alias",
            "--copy-from",
            "cursor:oauth-docs",
            "--scope",
            "project",
            "--description",
            "Copied documentation",
            "--header",
            "X-Added=${PARITY_UNSET_ADDED}",
        ],
    )
    .await?;
    let destination = fixture.project.join(".mcp").join("config.json");
    let document = read_config(&destination)?;
    let alias = server(&document, "alias")?;
    assert_eq!(
        alias.get("url"),
        Some(&json!("https://example.invalid/mcp"))
    );
    assert_eq!(alias.get("auth"), Some(&json!("oauth")));
    assert_eq!(
        alias.get("description"),
        Some(&json!("Copied documentation"))
    );
    assert_eq!(
        alias.pointer("/headers/X-Client"),
        Some(&json!("${PARITY_UNSET_HEADER}"))
    );
    assert_eq!(
        alias.pointer("/headers/X-Added"),
        Some(&json!("${PARITY_UNSET_ADDED}"))
    );
    assert!(document.pointer("/mcpServers/oauth-docs").is_none());
    assert!(!fixture.home_config().exists());
    assert_eq!(std::fs::read(&source)?, original_source);
    fixture.finish().await
}

#[tokio::test]
async fn import_does_not_read_sidecar_credentials_or_expand_ambient_values() -> Result<()> {
    let fixture = Fixture::new()?;
    let source = fixture.project.join(".vscode").join("mcp.json");
    let reference = format!("Bearer ${{{}}}", "PARITY_IMPORT_CANARY");
    fixture.write(
        &source,
        &json!({"servers":{"safe":{
            "type":"http",
            "url":"https://example.invalid/no-network",
            "headers":{"Authorization":reference,"X-Static":"parity-test-only-credential"}
        }}}),
    )?;
    write_source(
        &fixture.project.join(".vscode").join("credentials.json"),
        &format!(r#"{{"access_token":"{CANARY}"}}"#),
    )?;
    let output = success(
        &fixture,
        &["import", "vscode", "--scope", "project", "--json"],
    )
    .await?;
    let destination = fixture.project.join(".mcp").join("config.json");
    let document = read_config(&destination)?;
    assert_eq!(
        server(&document, "safe")?.pointer("/headers/Authorization"),
        Some(&json!(reference))
    );
    assert_eq!(
        server(&document, "safe")?.pointer("/headers/X-Static"),
        Some(&json!("parity-test-only-credential"))
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains("parity-test-only-credential"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("parity-test-only-credential"));
    assert!(
        !fixture
            .project
            .join(".mcp")
            .join("credentials.json")
            .exists()
    );
    assert!(
        !fixture
            .directory
            .join("pool")
            .join("config")
            .join("credentials.json")
            .exists()
    );
    no_canary(&output, &document)?;
    fixture.finish().await
}
