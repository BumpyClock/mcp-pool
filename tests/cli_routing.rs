#[path = "common/fixture_lifecycle.rs"]
mod fixture_lifecycle;

use std::io;

use serde_json::{Value, json};

#[path = "mcp_cli_fixtures/stdio.rs"]
mod stdio;
#[path = "mcp_cli_fixtures/support.rs"]
#[allow(dead_code)]
mod support;

use support::{Fixture, parse_json};

#[tokio::test]
async fn non_unicode_arguments_return_an_error_without_panicking() -> io::Result<()> {
    #[cfg(unix)]
    let argument = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![0xff])
    };
    #[cfg(windows)]
    let argument = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0xd800])
    };
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-cli"))
        .arg(argument)
        .output()
        .await?;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("command-line arguments must be valid Unicode"));
    assert!(!error.contains("panicked"));
    Ok(())
}

#[tokio::test]
async fn serve_help_describes_concurrency_without_loading_config() -> io::Result<()> {
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-cli"))
        .args([
            "--config",
            "missing-serve-help-config.json",
            "serve",
            "--help",
        ])
        .output()
        .await?;
    assert!(output.status.success(), "{output:?}");
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--concurrency 1..64"));
    assert!(help.contains("default 1"));
    Ok(())
}

async fn spawn_count(fixture: &Fixture) -> io::Result<usize> {
    if !tokio::fs::try_exists(&fixture.counter).await? {
        return Ok(0);
    }
    fixture.event_count("spawn").await
}

#[tokio::test]
async fn native_config_mutations_share_add_and_remove_behavior() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let original = tokio::fs::read(&fixture.config).await?;
    let preview = fixture
        .success(&[
            "add",
            "remote",
            "--url",
            "https://example.test/mcp",
            "--transport",
            "SSE",
            "--dry-run",
        ])
        .await?;
    let preview = parse_json(&preview)?;
    assert_eq!(preview.get("transport"), Some(&json!("sse")));
    assert_eq!(tokio::fs::read(&fixture.config).await?, original);

    fixture
        .success(&[
            "add",
            "native",
            "--transport",
            "ignored",
            "--",
            "fixture-command",
            "--some-flag",
            "argument with spaces",
        ])
        .await?;
    let native = parse_json(
        &fixture
            .success(&["config", "get", "native", "--json"])
            .await?,
    )?;
    assert_eq!(native.get("command"), Some(&json!("fixture-command")));
    assert_eq!(
        native.get("args"),
        Some(&json!(["--some-flag", "argument with spaces"]))
    );
    fixture
        .success(&[
            "config",
            "add",
            "config-entry",
            "--command",
            "fixture-command",
            "--arg",
            "--some-flag",
            "--arg",
            "argument with spaces",
        ])
        .await?;
    let document: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    assert_eq!(
        document.pointer("/mcpServers/native"),
        Some(&json!({
            "transport":"stdio",
            "command":"fixture-command",
            "args":["--some-flag", "argument with spaces"]
        }))
    );
    assert_eq!(
        document.pointer("/mcpServers/native"),
        document.pointer("/mcpServers/config-entry")
    );
    fixture.success(&["remove", "native", "--yes"]).await?;
    let missing = fixture
        .command(&["config", "get", "native", "--json"])
        .await?;
    assert!(!missing.status.success());
    fixture.finish().await
}

#[tokio::test]
async fn root_help_names_mcp_commands_and_native_pool_commands() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let output = fixture.success(&["--help"]).await?;
    let help = String::from_utf8_lossy(&output.stdout);
    for command in [
        "list",
        "call",
        "resource",
        "auth",
        "vault",
        "config",
        "import",
        "daemon",
        "serve",
        "pool list|serve",
    ] {
        assert!(help.contains(command), "missing {command} in {help}");
    }
    assert!(!help.contains("List configured servers (local config"));
    assert!(
        help.lines().any(|line| {
            line.trim_start().starts_with("serve ") && line.contains("Expose pooled")
        })
    );
    let native_help = fixture.success(&["pool", "--help"]).await?;
    let native_help = String::from_utf8_lossy(&native_help.stdout);
    assert!(native_help.contains("List configured servers without connecting"));
    assert!(!native_help.contains("Call an MCP tool"));
    let nested_call = fixture.command(&["pool", "call", "--help"]).await?;
    assert!(!nested_call.status.success());
    for (arguments, expected) in [
        (&["help", "call"][..], "Usage: mcp-cli call"),
        (&["help", "start"][..], "Start pooled MCP server"),
        (&["pool", "list", "--help"][..], "native pool entries"),
        (&["pool", "serve", "--help"][..], "native MCP pool daemon"),
    ] {
        let output = fixture.success(arguments).await?;
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(text.contains(expected), "{arguments:?}: {text}");
    }
    fixture.finish().await
}

#[tokio::test]
async fn inferred_server_forms_preserve_globals_and_call_payloads() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    let listed = parse_json(&fixture.success(&["--json", "fixture"]).await?)?;
    assert_eq!(listed.get("mode"), Some(&json!("server")));
    assert_eq!(listed.get("name"), Some(&json!("fixture")));

    let config = fixture.config.to_string_lossy().to_string();
    let listed_with_config = parse_json(
        &fixture
            .success(&["--config", &config, "--json", "fixture"])
            .await?,
    )?;
    assert_eq!(listed_with_config.get("name"), Some(&json!("fixture")));
    let listed_with_suffix = parse_json(&fixture.success(&["fixture", "list", "--json"]).await?)?;
    assert_eq!(listed_with_suffix.get("name"), Some(&json!("fixture")));
    let leading_json_call = parse_json(
        &fixture
            .success(&["--json", "fixture.echo", "label=leading"])
            .await?,
    )?;
    assert_eq!(
        leading_json_call.pointer("/arguments/label"),
        Some(&json!("leading"))
    );

    for (arguments, expected_label) in [
        (
            &["fixture.echo", "label=dotted", "--output", "json"][..],
            "dotted",
        ),
        (
            &["fixture", "echo", "label=separate", "--output", "json"][..],
            "separate",
        ),
    ] {
        let output = parse_json(&fixture.success(arguments).await?)?;
        assert_eq!(
            output.pointer("/arguments/label"),
            Some(&json!(expected_label))
        );
    }

    let expression = parse_json(
        &fixture
            .success(&["fixture.echo(label: \"expression\")", "--output", "json"])
            .await?,
    )?;
    assert_eq!(
        expression.pointer("/arguments/label"),
        Some(&json!("expression"))
    );
    fixture.finish().await
}

#[tokio::test]
async fn inferred_calls_keep_json_payloads_before_the_tool_name() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    for (arguments, expected) in [
        (
            &[
                "fixture",
                "--json",
                r#"{"label":"before"}"#,
                "echo",
                "--output",
                "json",
            ][..],
            "before",
        ),
        (
            &["--json", "fixture", "--json", r#"{"label":"both"}"#, "echo"][..],
            "both",
        ),
        (
            &[
                "fixture",
                "--mcp",
                "fixture",
                "--args",
                r#"{"label":"alias"}"#,
                "echo",
                "--output",
                "json",
            ][..],
            "alias",
        ),
    ] {
        let output = parse_json(&fixture.success(arguments).await?)?;
        assert_eq!(output.pointer("/arguments/label"), Some(&json!(expected)));
    }
    fixture.finish().await
}

#[tokio::test]
async fn exact_dotted_servers_and_explicit_native_commands_win() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    let fixture_server = configuration
        .pointer("/mcpServers/fixture")
        .cloned()
        .ok_or_else(|| io::Error::other("missing fixture server"))?;
    let mut dotted_server = fixture_server.clone();
    dotted_server
        .pointer_mut("/env")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("missing fixture environment"))?
        .insert("MCP_CLI_TEST_DOTTED_TOOL".to_owned(), json!("1"));
    let status_server = fixture_server.clone();
    let servers = configuration
        .pointer_mut("/mcpServers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("missing servers"))?;
    servers.insert("fixture.api".to_owned(), fixture_server);
    servers.insert("fixture.v2".to_owned(), dotted_server);
    servers.insert("status".to_owned(), status_server);
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    fixture.warm("fixture.v2").await?;
    fixture.warm("fixture.api").await?;

    let exact = parse_json(&fixture.success(&["fixture.v2", "--json"]).await?)?;
    assert_eq!(exact.get("name"), Some(&json!("fixture.v2")));
    let dotted_call = parse_json(
        &fixture
            .success(&["fixture.v2.echo.v2", "label=longest", "--output", "json"])
            .await?,
    )?;
    assert_eq!(
        dotted_call.pointer("/arguments/label"),
        Some(&json!("longest"))
    );
    let server_first = parse_json(
        &fixture
            .success(&[
                "fixture.v2",
                "echo.v2",
                "label=server-first",
                "--output",
                "json",
            ])
            .await?,
    )?;
    assert_eq!(
        server_first.pointer("/arguments/label"),
        Some(&json!("server-first"))
    );
    for expression in [
        "fixture.v2.echo.v2(label: \"dotted-expression\")",
        "echo.v2(label: \"dotted-expression\")",
    ] {
        let arguments = if expression.starts_with("fixture.") {
            vec![expression, "--output", "json"]
        } else {
            vec!["fixture.v2", expression, "--output", "json"]
        };
        let called = parse_json(&fixture.success(&arguments).await?)?;
        assert_eq!(
            called.pointer("/arguments/label"),
            Some(&json!("dotted-expression"))
        );
    }
    let expression = parse_json(
        &fixture
            .success(&[
                "fixture.api.echo(label: \"expression\")",
                "--output",
                "json",
            ])
            .await?,
    )?;
    assert_eq!(
        expression.pointer("/arguments/label"),
        Some(&json!("expression"))
    );

    // Explicit selectors split at the first dot. Inference above uses the longest configured prefix.
    let explicit = fixture
        .command(&[
            "call",
            "fixture.v2.echo.v2",
            "label=explicit",
            "--output",
            "json",
        ])
        .await?;
    assert!(!explicit.status.success());
    let explicit_error = parse_json(&explicit)?;
    assert_eq!(explicit_error.get("server"), Some(&json!("fixture")));
    assert_eq!(explicit_error.get("tool"), Some(&json!("v2.echo.v2")));
    let unambiguous = parse_json(
        &fixture
            .success(&[
                "call",
                "--server",
                "fixture.v2",
                "--tool",
                "echo.v2",
                "label=explicit-flags",
                "--output",
                "json",
            ])
            .await?,
    )?;
    assert_eq!(
        unambiguous.pointer("/arguments/label"),
        Some(&json!("explicit-flags"))
    );

    let before = fixture.event_count("spawn").await?;
    let native = fixture.success(&["status"]).await?;
    assert!(!String::from_utf8_lossy(&native.stdout).contains("\"mode\":\"server\""));
    assert_eq!(fixture.event_count("spawn").await?, before);
    fixture.finish().await
}

#[tokio::test]
async fn unknown_and_extra_tokens_fail_without_guessing() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let unknown = fixture.command(&["not-configured"]).await?;
    assert!(!unknown.status.success());
    let unknown_error = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        unknown_error.contains("unknown command or MCP server"),
        "{unknown_error}"
    );
    assert_eq!(spawn_count(&fixture).await?, 0);

    let extra = fixture.command(&["fixture", "list", "unexpected"]).await?;
    assert!(!extra.status.success());
    let extra_error = String::from_utf8_lossy(&extra.stderr);
    assert!(extra_error.contains("Usage: list"), "{extra_error}");
    assert_eq!(spawn_count(&fixture).await?, 0);
    fixture.finish().await
}

#[tokio::test]
async fn literal_help_and_quiet_do_not_override_mcp_dispatch() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    for marker in ["--help", "-h", "--quiet", "--plain", "--no-color"] {
        let output = parse_json(
            &fixture
                .success(&["call", "fixture.echo", "--output", "json", "--", marker])
                .await?,
        )?;
        assert_eq!(output.pointer("/arguments/label"), Some(&json!(marker)));
    }
    let inferred = parse_json(
        &fixture
            .success(&["fixture", "--output", "json", "--", "echo", "--help"])
            .await?,
    )?;
    assert_eq!(inferred.pointer("/arguments/label"), Some(&json!("--help")));
    let literal_tool = parse_json(
        &fixture
            .success(&["fixture", "--output", "json", "--", "--help"])
            .await?,
    )?;
    assert_eq!(literal_tool.get("name"), Some(&json!("fixture")));
    let literal_list = fixture
        .command(&["fixture", "--output", "json", "--", "list"])
        .await?;
    assert!(!literal_list.status.success());
    assert_eq!(parse_json(&literal_list)?.get("tool"), Some(&json!("list")));
    let help = fixture.success(&["help", "call", "--", "--quiet"]).await?;
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: mcp-cli call"));

    for marker in ["--help", "-h", "--quiet"] {
        let failed = fixture.command(&["list", "--", marker]).await?;
        assert!(!failed.status.success(), "{failed:?}");
        assert!(
            !failed.stderr.is_empty(),
            "literal marker suppressed an error: {marker}"
        );
        assert!(!String::from_utf8_lossy(&failed.stdout).contains("Usage: mcp-cli list"));
    }
    fixture.finish().await
}

#[tokio::test]
async fn global_values_and_literal_paths_do_not_reselect_configuration() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    let output = parse_json(
        &fixture
            .success(&[
                "--no-color",
                "--plain",
                "--log-level=error",
                "--json",
                "fixture",
                "echo",
                "--",
                "--config=missing.json",
            ])
            .await?,
    )?;
    assert_eq!(
        output.pointer("/arguments/label"),
        Some(&json!("--config=missing.json"))
    );
    let invalid_level = fixture
        .command(&["--log-level", "--config", "fixture"])
        .await?;
    assert!(!invalid_level.status.success());
    assert!(String::from_utf8_lossy(&invalid_level.stderr).contains("--log-level must be"));
    assert!(!String::from_utf8_lossy(&invalid_level.stderr).contains("requires a path"));
    fixture.finish().await
}

#[tokio::test]
async fn oauth_timeout_remains_an_authorization_only_global() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    for prefix in ["--oauth-timeout=10", "--oauth-timeout"] {
        let mut arguments = vec![prefix];
        if prefix == "--oauth-timeout" {
            arguments.push("10");
        }
        arguments.extend(["fixture", "--json"]);
        let rejected = fixture.command(&arguments).await?;
        assert!(!rejected.status.success());
        assert!(
            String::from_utf8_lossy(&rejected.stderr)
                .contains("applies only to auth or config login")
        );
    }
    let help = fixture
        .success(&["--oauth-timeout=10", "auth", "--help"])
        .await?;
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: mcp-cli auth"));
    assert_eq!(spawn_count(&fixture).await?, 0);
    fixture.finish().await
}

#[tokio::test]
async fn native_add_rejects_empty_url_without_changing_config() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let original = tokio::fs::read(&fixture.config).await?;
    let rejected = fixture.command(&["add", "empty", "--url", ""]).await?;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("--url must not be empty"));
    assert_eq!(tokio::fs::read(&fixture.config).await?, original);
    assert_eq!(spawn_count(&fixture).await?, 0);
    fixture.finish().await
}
