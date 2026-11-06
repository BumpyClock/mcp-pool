use super::*;

#[test]
fn validation_and_reading_share_url_alias_precedence() -> Result<()> {
    for field in ["baseUrl", "base_url", "url", "serverUrl", "server_url"] {
        let document = json!({"mcpServers":{"test":{
            field:"https://selected.invalid/mcp",
            "transport":"streamable-http"
        }}});
        validate_new_entries(&document)?;
        validate_document(&document)?;
        let configuration = parse(&document.to_string())?;
        assert_eq!(
            server(&configuration)?.definition.url,
            "https://selected.invalid/mcp"
        );
        assert_eq!(server(&configuration)?.definition.transport, "http");
    }
    let document = json!({"mcpServers":{"test":{
        "baseUrl":"https://selected.invalid/mcp",
        "url":"invalid-ignored-url"
    }}});
    validate_new_entries(&document)?;
    assert_eq!(
        server(&parse(&document.to_string())?)?.definition.url,
        "https://selected.invalid/mcp"
    );
    Ok(())
}

#[test]
fn preserving_compatible_entries_is_distinct_from_validating_new_entries() -> Result<()> {
    for entry in [
        json!({"baseUrl":"https://selected.invalid/mcp", "command":"ignored", "transport":"http"}),
        json!({"baseUrl":"", "url":"https://ignored.invalid/mcp", "command":"echo"}),
    ] {
        let document = json!({"mcpServers":{"test":entry}});
        validate_document(&document)?;
        parse(&document.to_string())?;
        assert!(validate_new_entries(&document).is_err());
    }
    let deferred = json!({"mcpServers":{"test":{
        "url":"https://${MCP_CLI_SYNTHETIC_MISSING}/mcp",
        "headers":{"Authorization":"${MCP_CLI_SYNTHETIC_MISSING}"},
        "oauthClientId":7,
        "oauth_client_id":"client"
    }}});
    validate_new_entries(&deferred)?;
    validate_document(&deferred)?;
    assert!(parse(&deferred.to_string()).is_err());
    Ok(())
}

#[test]
fn urls_aliases_and_remote_transport_are_normalized() -> Result<()> {
    let configuration = parse(
        r#"{"mcpServers":{"test":{
            "baseUrl":"https://example.invalid/mcp",
            "url":"https://ignored.invalid/mcp",
            "command":"ignored",
            "transport":"sse",
            "timeoutMs":1500,
            "description":"synthetic",
            "auth":"oauth",
            "clientName":"synthetic-client",
            "lifecycle":"keep-alive"
        }}}"#,
    )?;
    let entry = server(&configuration)?;
    assert_eq!(entry.definition.url, "https://example.invalid/mcp");
    assert_eq!(entry.definition.transport, "sse");
    assert_eq!(entry.definition.timeout_ms, Some(1500));
    assert_eq!(entry.definition.description, "synthetic");
    assert!(entry.definition.command.is_empty());
    assert!(entry.definition.cwd.is_none());
    assert!(entry.definition.auth.is_none());
    assert_eq!(
        entry.raw.get("clientName").and_then(Value::as_str),
        Some("synthetic-client")
    );
    Ok(())
}

#[test]
fn derived_settings_preserve_permissive_aliases_and_follow_raw_changes() -> Result<()> {
    let configuration = parse(
        r#"{"mcpServers":{"test":{
            "command":"node",
            "oauthClientSecret": 7,
            "oauth_client_secret":"snake-secret",
            "oauthRequestedScope":" ",
            "oauth_requested_scope":"requested-snake",
            "oauthScope":"fallback-camel",
            "oauth_scope":"fallback-snake",
            "oauthCommand":null,
            "oauth_command":{"args":["ignored"]},
            "lifecycle":{"mode":"keep-alive"},
            "env":{"Configured":"value"},
            "custom":{"nested":true}
        }}}"#,
    )?;
    let mut entry = server(&configuration)?.clone();
    let settings = entry.oauth_settings();
    assert_eq!(settings.client_secret, Some("snake-secret"));
    assert_eq!(settings.scope, Some("requested-snake"));
    assert!(settings.helper_arguments.is_none());
    assert_eq!(entry.configured_lifecycle(), Some(true));
    let environment = entry.configured_environment();
    assert!(environment.contains_exact("Configured"));
    assert!(!environment.contains_exact("CONFIGURED"));
    assert_eq!(environment.contains("CONFIGURED"), cfg!(windows));
    assert_eq!(entry.raw.pointer("/custom/nested"), Some(&json!(true)));

    let raw = entry.raw.as_object_mut().context("raw server entry")?;
    raw.insert("oauth_requested_scope".to_owned(), Value::Null);
    raw.insert("oauthCommand".to_owned(), json!({"args":["actual", 5]}));
    raw.insert("lifecycle".to_owned(), json!({"mode":"ephemeral"}));
    let settings = entry.oauth_settings();
    assert_eq!(settings.scope, Some("fallback-camel"));
    let expected_arguments = [json!("actual"), json!(5)];
    assert_eq!(
        settings.helper_arguments,
        Some(expected_arguments.as_slice())
    );
    assert_eq!(entry.configured_lifecycle(), Some(false));
    Ok(())
}

#[test]
fn url_and_headers_interpolate_without_echoing_values() -> Result<()> {
    let configuration = parse_with_environment(
        &source()?,
        r#"{"mcpServers":{"test":{"base_url":"https://${HOST}/mcp","headers":{"X-Test":"$env:VALUE"}}}}"#,
        &|name| {
            Ok(Some(
                if name == "HOST" {
                    "example.invalid"
                } else {
                    "synthetic"
                }
                .into(),
            ))
        },
        &|| Ok(BTreeMap::new()),
    )?;
    assert_eq!(
        server(&configuration)?.definition.url,
        "https://example.invalid/mcp"
    );
    assert_eq!(
        server(&configuration)?
            .definition
            .headers
            .get("X-Test")
            .map(String::as_str),
        Some("synthetic")
    );
    Ok(())
}

#[test]
fn command_array_uses_its_own_arguments() -> Result<()> {
    let configuration = parse(
        r#"{"imports":[],"mcpServers":{"test":{"command":["node","server.js","--flag"],"args":["ignored"]}}}"#,
    )?;
    let definition = &server(&configuration)?.definition;
    assert_eq!(definition.command, "node");
    assert_eq!(definition.args, ["server.js", "--flag"]);
    assert_eq!(definition.cwd, source()?.parent().map(Path::to_path_buf));
    assert!(definition.auth.is_none());
    Ok(())
}

#[test]
fn command_string_supports_quotes_and_empty_arguments() -> Result<()> {
    let configuration =
        parse(r#"{"mcpServers":{"test":{"command":"node --name 'two words' \"\" --enabled"}}}"#)?;
    assert_eq!(server(&configuration)?.definition.command, "node");
    assert_eq!(
        server(&configuration)?.definition.args,
        ["--name", "two words", "", "--enabled"]
    );
    Ok(())
}

#[test]
fn explicit_args_do_not_split_command() -> Result<()> {
    let configuration = parse(
        r#"{"mcpServers":{"test":{"command":"node executable","args":["--name","two words"]}}}"#,
    )?;
    assert_eq!(
        server(&configuration)?.definition.command,
        "node executable"
    );
    assert_eq!(
        server(&configuration)?.definition.args,
        ["--name", "two words"]
    );
    Ok(())
}

#[test]
fn preserving_transport_references_does_not_require_the_callers_environment() -> Result<()> {
    let document = json!({"mcpServers":{"test":{
        "url":"https://example.invalid/mcp",
        "transport":"${REMOTE_TRANSPORT}"
    }}});
    validate_document(&document)?;
    let configuration = parse_with_environment(
        &source()?,
        &document.to_string(),
        &|name| Ok((name == "REMOTE_TRANSPORT").then(|| "sse".to_owned())),
        &|| Ok(BTreeMap::new()),
    )?;
    assert_eq!(server(&configuration)?.definition.transport, "sse");
    Ok(())
}
