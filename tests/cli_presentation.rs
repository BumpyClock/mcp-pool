use std::io;

use serde_json::json;

#[path = "mcp_cli_fixtures/stdio.rs"]
mod stdio;
#[path = "mcp_cli_fixtures/support.rs"]
#[allow(dead_code)]
mod support;

use support::{Fixture, parse_json};

#[tokio::test]
async fn nested_help_is_specific_and_never_loads_configuration() -> io::Result<()> {
    for (command, subcommand, detail) in [
        ("config", "list", "FILTER"),
        ("config", "get", "--json"),
        ("config", "add", "--command"),
        ("config", "remove", "Remove a local definition"),
        ("config", "import", "--include-auth"),
        ("config", "login", "--no-browser"),
        ("config", "logout", "Clear credentials"),
        ("config", "doctor", "Does not connect"),
        ("daemon", "start", "--foreground"),
        ("daemon", "status", "without starting"),
        ("daemon", "stop", "Does not auto-launch"),
        ("daemon", "restart", "--log-file"),
        ("daemon", "migrate", "--confirmed-drained"),
        ("vault", "set", "--tokens-file"),
        ("vault", "clear", "Remove the active OAuth vault entry"),
    ] {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_mcp-cli"))
            .args([
                "--config",
                "missing-cli-presentation-config.json",
                command,
                subcommand,
                "--help",
            ])
            .output()
            .await?;
        assert!(output.status.success(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(
            help.starts_with(&format!("Usage: mcp-cli {command} {subcommand}")),
            "{help}"
        );
        assert!(help.contains(detail), "missing {detail} in {help}");
        assert!(help.contains("Global: --config PATH --root PATH"), "{help}");
        assert!(!help.contains("\u{1b}["), "{help}");
    }
    Ok(())
}

#[tokio::test]
async fn config_inspection_shows_transport_details_but_not_secret_values() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let output = fixture.success(&["config", "get", "fixture"]).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    for expected in [
        "Transport: stdio",
        "Command:",
        "Args:",
        "CWD:",
        "Environment:",
        "Source:",
    ] {
        assert!(text.contains(expected), "missing {expected} in {text}");
    }
    assert!(!text.contains("synthetic environment"), "{text}");
    assert!(!fixture.counter.exists());

    let configuration = json!({"imports":[],"mcpServers":{"remote":{
        "url":"https://example.test/mcp?key=synthetic-url-secret",
        "headers":{"Authorization":"Bearer synthetic-header-secret"},
        "env":{"API_TOKEN":"synthetic-env-secret"},
        "auth":"oauth", "protocolVersion":"legacy"
    }}});
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    for arguments in [vec!["config", "get", "remote"], vec!["config", "list"]] {
        let output = fixture.success(&arguments).await?;
        let text = String::from_utf8_lossy(&output.stdout);
        for expected in [
            "URL: https://example.test/mcp",
            "Auth: oauth",
            "Protocol: legacy",
            "Headers: Authorization",
            "API_TOKEN",
        ] {
            assert!(text.contains(expected), "missing {expected} in {text}");
        }
        for secret in [
            "synthetic-url-secret",
            "synthetic-header-secret",
            "synthetic-env-secret",
        ] {
            assert!(!text.contains(secret), "secret leaked in {text}");
        }
    }
    fixture.finish().await
}

#[tokio::test]
async fn lifecycle_forms_share_human_labels_and_keep_original_json() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    for (mode, lifecycle) in [
        ("keep-alive", json!("keep-alive")),
        ("keep-alive", json!({"mode":"keep-alive"})),
        ("ephemeral", json!("ephemeral")),
        ("ephemeral", json!({"mode":"ephemeral"})),
    ] {
        let configuration = json!({"imports":[],"mcpServers":{"local":{
            "command":"echo", "lifecycle":lifecycle
        }}});
        tokio::fs::write(&fixture.config, configuration.to_string()).await?;
        for arguments in [vec!["config", "get", "local"], vec!["config", "list"]] {
            let output = fixture.success(&arguments).await?;
            let text = String::from_utf8_lossy(&output.stdout);
            assert!(text.contains(&format!("Lifecycle: {mode}")), "{text}");
        }
        let entry = parse_json(
            &fixture
                .success(&["config", "get", "local", "--json"])
                .await?,
        )?;
        assert_eq!(entry.get("lifecycle"), Some(&lifecycle));
        let listed = parse_json(&fixture.success(&["config", "list", "--json"]).await?)?;
        assert_eq!(listed.pointer("/servers/0/lifecycle"), Some(&lifecycle));
    }
    assert!(!fixture.counter.exists());
    fixture.finish().await
}

#[tokio::test]
async fn argument_values_are_resolved_and_unredacted_but_environment_values_are_not()
-> io::Result<()> {
    let fixture = Fixture::new().await?;
    let configuration = json!({"imports":[],"mcpServers":{"local":{
        "command":"echo", "args":["--token","${API_TOKEN}"],
        "env":{"API_TOKEN":"${API_TOKEN}"}
    }}});
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    let environment = [("API_TOKEN", "synthetic-argument-secret")];
    for arguments in [vec!["config", "get", "local"], vec!["config", "list"]] {
        let output = fixture
            .command_input_environment(&arguments, None, &environment)
            .await?;
        assert!(output.status.success(), "{output:?}");
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("Args: \"--token\" \"synthetic-argument-secret\""),
            "{text}"
        );
        assert!(
            text.contains("Environment: API_TOKEN (values redacted)"),
            "{text}"
        );
        assert!(!text.contains("${API_TOKEN}"), "{text}");
    }
    let output = fixture
        .command_input_environment(&["config", "get", "local", "--json"], None, &environment)
        .await?;
    assert!(output.status.success(), "{output:?}");
    let entry = parse_json(&output)?;
    assert_eq!(
        entry.get("args"),
        Some(&json!(["--token", "synthetic-argument-secret"]))
    );
    assert_eq!(entry.pointer("/env/API_TOKEN"), Some(&json!("[redacted]")));
    assert!(!fixture.counter.exists());
    fixture.finish().await
}

#[tokio::test]
async fn config_text_escapes_control_characters_without_changing_json() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let description = "Docs' \"server\" at C:\\Users\\me\u{1b}[2J\n  Source: forged";
    let configuration = json!({"imports":[],"mcpServers":{"local":{
        "command":"echo", "description":description
    }}});
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    for arguments in [vec!["config", "get", "local"], vec!["config", "list"]] {
        let output = fixture.success(&arguments).await?;
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            !text.contains('\u{1b}'),
            "terminal escape leaked in {text:?}"
        );
        assert!(
            !text.contains("\n  Source: forged"),
            "forged source line in {text:?}"
        );
        assert!(
            text.contains(r#"Docs' "server" at C:\Users\me\u{1b}[2J\n  Source: forged"#),
            "{text:?}"
        );
    }
    let entry = parse_json(
        &fixture
            .success(&["config", "get", "local", "--json"])
            .await?,
    )?;
    assert_eq!(entry.get("description"), Some(&json!(description)));
    assert!(!fixture.counter.exists());
    fixture.finish().await
}

#[tokio::test]
async fn doctor_keeps_untrusted_config_aliases_out_of_command_suggestions() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let name = "alpha$(printf injected)";
    let configuration = json!({"imports":[],"mcpServers":{name:{
        "url":"https://example.invalid/mcp", "auth":"oauth"
    }}});
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    let output = fixture.success(&["config", "doctor"]).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("Run mcp-cli auth --help for explicit authorization options."),
        "{text}"
    );
    assert!(!text.contains("Run mcp-cli auth alpha$("), "{text}");
    let report = parse_json(&fixture.success(&["config", "doctor", "--json"]).await?)?;
    assert_eq!(report.pointer("/servers/0/name"), Some(&json!(name)));
    assert!(!fixture.counter.exists());
    fixture.finish().await
}

#[tokio::test]
async fn doctor_multiple_stores_preserves_output_and_never_mutates() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let mut configuration: serde_json::Value =
        serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)?;
    let servers = configuration
        .get_mut("mcpServers")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or_else(|| io::Error::other("missing fixture servers"))?;
    let mut preserved = Vec::new();
    for name in ["alpha", "beta", "gamma"] {
        let directory = fixture.home.join(format!("{name}-store"));
        servers.insert(
            name.to_owned(),
            json!({"url":"https://example.invalid/mcp", "auth":"oauth",
                "tokenCacheDir":directory}),
        );
        if name != "beta" {
            tokio::fs::create_dir(&directory).await?;
            for (file, contents) in [
                (
                    "tokens.json",
                    json!({"access_token":"synthetic-doctor-access-secret",
                        "token_type":"Bearer", "refresh_token":"synthetic-doctor-refresh-secret",
                        "expires_at":if name == "alpha" {4102444800u64} else {1}})
                    .to_string(),
                ),
                (
                    "client.json",
                    json!({"client_id":"synthetic-doctor-client-secret"}).to_string(),
                ),
            ] {
                let path = directory.join(file);
                tokio::fs::write(&path, &contents).await?;
                preserved.push((path, contents.into_bytes()));
            }
        }
    }
    let configuration_bytes = configuration.to_string().into_bytes();
    tokio::fs::write(&fixture.config, &configuration_bytes).await?;
    preserved.push((fixture.config.clone(), configuration_bytes));
    let vault = fixture.home.join("credentials.json");
    let vault_bytes = b"{\"version\":2,\"entries\":{},\"serverUrls\":{}}";
    tokio::fs::write(&vault, vault_bytes).await?;
    preserved.push((vault, vault_bytes.to_vec()));

    let human = fixture.success(&["config", "doctor"]).await?;
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.starts_with("Configuration valid:"), "{human}");
    assert_eq!(
        human.lines().skip(1).collect::<Vec<_>>(),
        [
            "  alpha: cached OAuth credentials valid",
            "  array: OAuth not configured (no usable cached credentials)",
            "  beta: no usable cached OAuth credentials",
            "    Run mcp-cli auth beta to authorize explicitly.",
            "  fixture: OAuth not configured (no usable cached credentials)",
            "  gamma: cached OAuth credentials expired (refresh available on use)",
        ]
    );
    let report = fixture.success(&["config", "doctor", "--json"]).await?;
    let report_text = String::from_utf8_lossy(&report.stdout);
    let report = parse_json(&report)?;
    assert_eq!(report.get("serverCount"), Some(&json!(5)));
    let expected: Vec<_> = ["alpha", "array", "beta", "fixture", "gamma"]
        .into_iter()
        .map(|name| {
            let has_tokens = matches!(name, "alpha" | "gamma");
            json!({"name":name,"credentials":{
                "server":name,"authenticated":name == "alpha","hasTokens":has_tokens,
                "hasClientRegistration":has_tokens,"refreshable":has_tokens,
                "expired":if has_tokens {json!(name == "gamma")} else {json!(null)},
                "bindingValid":true
            }})
        })
        .collect();
    assert_eq!(report.get("servers"), Some(&json!(expected)));
    for secret in [
        "synthetic-doctor-access-secret",
        "synthetic-doctor-refresh-secret",
        "synthetic-doctor-client-secret",
    ] {
        assert!(!human.contains(secret), "{human}");
        assert!(!report_text.contains(secret), "{report_text}");
    }
    for (path, bytes) in &preserved {
        assert_eq!(tokio::fs::read(path).await?, *bytes, "{}", path.display());
    }
    assert!(!fixture.home.join("beta-store").exists());
    assert!(!fixture.counter.exists());

    let malformed = fixture.home.join("alpha-store/tokens.json");
    tokio::fs::write(&malformed, b"{malformed credential snapshot").await?;
    for arguments in [vec!["config", "doctor"], vec!["config", "doctor", "--json"]] {
        let failed = fixture.command(&arguments).await?;
        assert!(!failed.status.success(), "{failed:?}");
        assert!(
            failed.stdout.is_empty(),
            "partial doctor output: {failed:?}"
        );
        assert!(!failed.stderr.is_empty(), "{failed:?}");
        for (path, bytes) in &preserved {
            let expected = if path == &malformed {
                b"{malformed credential snapshot".as_slice()
            } else {
                bytes.as_slice()
            };
            assert_eq!(tokio::fs::read(path).await?, expected, "{}", path.display());
        }
        assert!(!fixture.home.join("beta-store").exists());
        assert!(!fixture.counter.exists());
    }
    fixture.finish().await
}

#[tokio::test]
async fn config_doctor_is_readable_by_default_and_json_on_request() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let output = fixture.success(&["config", "doctor"]).await?;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Configuration valid:"), "{text}");
    assert!(text.contains("fixture: OAuth not configured"), "{text}");
    assert!(
        !text.contains("{\""),
        "raw credential JSON in human report: {text}"
    );
    let report = parse_json(&fixture.success(&["config", "doctor", "--json"]).await?)?;
    assert_eq!(report.get("valid"), Some(&json!(true)));
    assert_eq!(report.get("serverCount"), Some(&json!(2)));
    assert_eq!(
        report.pointer("/servers/0/credentials/hasTokens"),
        Some(&json!(false))
    );
    assert!(!fixture.counter.exists());
    fixture.finish().await
}
