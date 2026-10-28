use super::*;
use serde_json::json;

#[test]
fn codex_conversion_keeps_dynamic_credentials_and_tool_permissions() -> Result<()> {
    let mut warnings = Vec::new();
    let entry = convert::entry(
        ImportKind::Codex,
        &json!({
            "url":"https://example.invalid/mcp",
            "http_headers":{"X-Static":"value"},
            "env_http_headers":{"X-Key":"PRIVATE_KEY"},
            "bearer_token_env_var":"PRIVATE_TOKEN",
            "enabled_tools":["read","write"],"disabled_tools":["write"],
            "tool_timeout_sec":1.25
        }),
        "fixture",
        &mut warnings,
    )?
    .context("entry")?;
    assert_eq!(
        entry.get("headers"),
        Some(&json!({"X-Static":"value","X-Key":"$env:PRIVATE_KEY"}))
    );
    assert_eq!(entry.get("bearerTokenEnv"), Some(&json!("PRIVATE_TOKEN")));
    assert_eq!(entry.get("allowedTools"), Some(&json!(["read"])));
    assert!(entry.get("blockedTools").is_none());
    assert_eq!(entry.get("timeoutMs"), Some(&json!(1250)));
    crate::server_config::validate_document(&json!({"mcpServers":{"fixture":entry}}))?;
    Ok(())
}

#[test]
fn import_preserves_claude_sse_and_literal_command_tokens() -> Result<()> {
    let mut warnings = Vec::new();
    let remote = convert::entry(
        ImportKind::Claude,
        &json!({"type":"sse","url":"https://example.invalid/events"}),
        "remote",
        &mut warnings,
    )?
    .context("entry")?;
    assert_eq!(remote.get("transport"), Some(&json!("sse")));
    let local = convert::entry(
        ImportKind::Claude,
        &json!({"command":"C:\\Program Files\\server.exe","args":["${TOKEN}",""]}),
        "local",
        &mut warnings,
    )?
    .context("entry")?;
    assert_eq!(
        local.get("command"),
        Some(&json!(["C:\\Program Files\\server.exe", "${TOKEN}", ""]))
    );
    Ok(())
}

#[test]
fn unsupported_auth_helpers_and_transports_are_not_silently_downgraded() {
    for (kind, entry) in [
        (
            ImportKind::Claude,
            json!({"type":"ws","url":"wss://example.invalid"}),
        ),
        (
            ImportKind::Claude,
            json!({"command":"fixture","headersHelper":"private-helper"}),
        ),
        (
            ImportKind::Codex,
            json!({"url":"https://example.invalid","auth":"chatgpt"}),
        ),
        (
            ImportKind::Codex,
            json!({"url":"https://example.invalid","http_headers_helper":"private-helper"}),
        ),
    ] {
        let error = convert::entry(kind, &entry, "fixture", &mut Vec::new())
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(!error.is_empty());
        assert!(!error.contains("private-helper"));
    }
}

#[test]
fn disabled_codex_entries_are_reported() -> Result<()> {
    let mut warnings = Vec::new();
    assert!(
        convert::entry(
            ImportKind::Codex,
            &json!({"command":"fixture","enabled":false}),
            "fixture",
            &mut warnings
        )?
        .is_none()
    );
    assert_eq!(warnings.len(), 1);
    Ok(())
}

#[test]
fn bearer_token_references_survive_and_static_credentials_are_rejected() -> Result<()> {
    let mut warnings = Vec::new();
    let entry = convert::entry(
        ImportKind::Cursor,
        &json!({"url":"https://example.invalid","bearerToken":"{env:TEST_BEARER_TOKEN}"}),
        "fixture",
        &mut warnings,
    )?
    .context("entry")?;
    assert_eq!(
        entry.get("bearerToken"),
        Some(&json!("$env:TEST_BEARER_TOKEN"))
    );
    let error = convert::entry(
        ImportKind::Cursor,
        &json!({"url":"https://example.invalid","bearerToken":"test-static-token"}),
        "fixture",
        &mut Vec::new(),
    )
    .expect_err("static bearer credentials must not be imported");
    assert!(error.to_string().contains("bearerTokenEnv"));
    let error = convert::entry(
        ImportKind::Codex,
        &json!({"url":"https://example.invalid","oauth_client_secret":"test-client-secret"}),
        "fixture",
        &mut Vec::new(),
    )
    .expect_err("direct OAuth secrets must not be imported");
    assert!(error.to_string().contains("oauth_client_secret"));
    let error = convert::entry(
        ImportKind::Windsurf,
        &json!({"url":"https://example.invalid","headers":{"Authorization":"******"}}),
        "fixture",
        &mut Vec::new(),
    )
    .expect_err("masked authorization must not be persisted");
    assert!(error.to_string().contains("masked Authorization"));
    Ok(())
}
