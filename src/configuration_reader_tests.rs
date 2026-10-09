use super::*;

fn source() -> Result<PathBuf> {
    Ok(std::env::current_dir()?
        .join("synthetic-config")
        .join("mcporter.json"))
}

fn parse(contents: &str) -> Result<ServerConfiguration> {
    parse_with_environment(&source()?, contents, &|_| Ok(None), &|| Ok(BTreeMap::new()))
}

fn server(configuration: &ServerConfiguration) -> Result<&ConfiguredServer> {
    configuration
        .servers
        .get("test")
        .ok_or_else(|| anyhow!("synthetic test server not found"))
}

struct Fixture {
    directory: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let identifier = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let parent = std::env::current_dir()?.join("target");
        std::fs::create_dir_all(&parent)?;
        let directory = parent.join(format!(
            "mcporter config fixture {}-{identifier}",
            std::process::id()
        ));
        std::fs::create_dir(&directory)?;
        Ok(Self { directory })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory) {
            eprintln!("could not remove synthetic config fixture: {error}");
        }
    }
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
fn windows_command_paths_keep_backslashes() -> Result<()> {
    let configuration = parse(
        r#"{"mcpServers":{"test":{"command":"\"C:\\Program Files\\MCP\\server.exe\" --root C:\\work\\data"}}}"#,
    )?;
    assert_eq!(
        server(&configuration)?.definition.command,
        r"C:\Program Files\MCP\server.exe"
    );
    assert_eq!(
        server(&configuration)?.definition.args,
        ["--root", r"C:\work\data"]
    );
    Ok(())
}

#[test]
fn relative_paths_use_the_source_directory() -> Result<()> {
    let source = source()?;
    let directory = source
        .parent()
        .ok_or_else(|| anyhow!("test source has no parent"))?;
    let configuration = parse(
        r#"{"mcpServers":{"test":{"command":["./scripts/server","argument"],"cwd":"workers"}}}"#,
    )?;
    let definition = &server(&configuration)?.definition;
    assert_eq!(definition.cwd, Some(directory.join("workers")));
    assert_eq!(
        PathBuf::from(&definition.command),
        directory.join("./scripts/server")
    );
    assert_eq!(server(&configuration)?.source, source);
    assert_eq!(configuration.source, source);
    Ok(())
}

#[test]
fn absolute_windows_cwd_is_not_prefixed() -> Result<()> {
    let configuration =
        parse(r#"{"mcpServers":{"test":{"command":["node"],"cwd":"C:\\work\\mcp"}}}"#)?;
    assert_eq!(
        server(&configuration)?.definition.cwd,
        Some(PathBuf::from(r"C:\work\mcp"))
    );
    Ok(())
}

#[test]
fn jsonc_comments_trailing_commas_and_bom_are_supported() -> Result<()> {
    let configuration = parse(
        "\u{feff}{ // config\n\"mcpServers\": { \"test\": { \"command\": [\"node\",], }, }, /* end */ \"imports\": [], }",
    )?;
    assert_eq!(configuration.servers.len(), 1);
    assert!(configuration.warnings.is_empty());
    Ok(())
}

#[test]
fn environment_expands_in_supported_fields() -> Result<()> {
    let configuration = parse_with_environment(
        &source()?,
        r#"{"mcpServers":{"test":{
            "command":["${EXECUTABLE}","${OPTION:-default}"],
            "env":{"VALUE":"$env:VALUE","OTHER":"prefix-${OPTION:-suffix}"},
            "headers":{"X-Test":"${VALUE}","Accept":"application/json"},
            "cwd":"${DIRECTORY:-workers}",
            "description":"uses ${EXECUTABLE}"
        }}}"#,
        &|name| {
            Ok(match name {
                "EXECUTABLE" => Some("node".into()),
                "VALUE" => Some("synthetic".into()),
                _ => None,
            })
        },
        &|| Ok(BTreeMap::new()),
    )?;
    let definition = &server(&configuration)?.definition;
    assert_eq!(definition.command, "node");
    assert_eq!(definition.args, ["default"]);
    assert_eq!(
        definition.env.get("VALUE").map(String::as_str),
        Some("synthetic")
    );
    assert_eq!(
        definition.env.get("OTHER").map(String::as_str),
        Some("prefix-suffix")
    );
    assert_eq!(
        definition.headers.get("X-Test").map(String::as_str),
        Some("synthetic")
    );
    assert_eq!(definition.description, "uses node");
    assert_eq!(
        server(&configuration)?.raw.get("command"),
        Some(&serde_json::json!(["${EXECUTABLE}", "${OPTION:-default}"]))
    );
    Ok(())
}

#[test]
fn missing_environment_variables_are_explicit_errors() -> Result<()> {
    for field in ["env", "headers"] {
        let contents = format!(
            "{{\"mcpServers\":{{\"test\":{{\"command\":\"node\",\"{field}\":{{\"value\":\"${{MCP_POOL_SYNTHETIC_MISSING}}\"}}}}}}}}"
        );
        let error = parse(&contents)
            .err()
            .ok_or_else(|| anyhow!("missing variable was accepted"))?;
        let message = format!("{error:#}");
        assert!(message.contains("MCP_POOL_SYNTHETIC_MISSING"));
        assert!(message.contains("missing"));
        assert!(message.contains(field));
    }
    Ok(())
}

#[test]
fn environment_defaults_and_empty_values_match_reference() -> Result<()> {
    let empty = |_: &str| Ok(Some(String::new()));
    assert_eq!(
        values::expand_environment("${VALUE:-fallback}", &empty)?,
        "fallback"
    );
    assert_eq!(values::expand_environment("${VALUE}", &empty)?, "");
    assert_eq!(values::expand_environment("$env:VALUE", &empty)?, "");
    assert_eq!(
        values::expand_environment(r"prefix-\${VALUE:-fallback}", &|_| Ok(None))?,
        "prefix-fallback"
    );
    assert!(values::expand_environment("${env:VALUE}", &empty).is_err());
    assert!(values::expand_environment("$env:VALUE embedded", &empty).is_err());
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
fn deferred_imports_have_source_diagnostics() -> Result<()> {
    let configuration =
        parse(r#"{"imports":["cursor","claude-code"],"mcpServers":{"test":{"command":"node"}}}"#)?;
    assert_eq!(configuration.servers.len(), 1);
    assert_eq!(configuration.warnings.len(), 1);
    let warning = configuration
        .warnings
        .first()
        .ok_or_else(|| anyhow!("warning missing"))?;
    assert!(warning.contains("2 import(s)"));
    assert!(warning.contains("deferred"));
    assert!(warning.contains(&configuration.source.to_string_lossy().to_string()));
    assert_eq!(parse(r#"{"mcpServers":{}}"#)?.warnings.len(), 1);
    assert!(
        parse(r#"{"imports":[],"mcpServers":{}}"#)?
            .warnings
            .is_empty()
    );
    Ok(())
}

#[test]
fn malformed_config_and_entries_fail_without_values_in_errors() -> Result<()> {
    for contents in [
        "",
        "[]",
        "{}",
        "{\"mcpServers\":",
        r#"{"mcpServers":{} "imports":[]}"#,
        r#"{mcpServers:{}}"#,
        r#"{"mcpServers":{"test":{"command":[]}}}"#,
        r#"{"mcpServers":{"test":{"command":"\"unclosed"}}}"#,
        r#"{"mcpServers":{"test":{"command":"node","headers":{"X-Test":123}}}}"#,
        r#"{"mcpServers":{"test":{"command":"node","timeoutMs":-1}}}"#,
        r#"{"mcpServers":{"test":{"url":"synthetic-sensitive-value"}}}"#,
        r#"{"mcpServers":{},"imports":[123]}"#,
        r#"{"mcpServers":{"test":{"command":123,"description":"synthetic-sensitive-value"}}}"#,
    ] {
        let error = parse(contents)
            .err()
            .ok_or_else(|| anyhow!("invalid configuration accepted"))?;
        assert!(!format!("{error:#}").contains("synthetic-sensitive-value"));
    }
    Ok(())
}

#[test]
fn explicit_load_reports_selected_file() -> Result<()> {
    let path = std::env::current_dir()?
        .join("src")
        .join("configuration_reader_tests.rs");
    let error = load(Some(path.clone()))
        .err()
        .ok_or_else(|| anyhow!("Rust test source unexpectedly parsed as JSONC"))?;
    assert!(format!("{error:#}").contains(&path.to_string_lossy().to_string()));
    Ok(())
}

#[test]
fn explicit_load_reads_only_the_selected_source() -> Result<()> {
    let fixture = Fixture::new()?;
    let selected = fixture.directory.join("mcporter.json");
    std::fs::write(
        &selected,
        r#"{"imports":[],"mcpServers":{"test":{"command":["node"],"cwd":"workers"}}}"#,
    )?;
    let configuration = load(Some(selected.clone()))?;
    assert_eq!(configuration.source, selected);
    assert_eq!(configuration.servers.len(), 1);
    assert_eq!(
        server(&configuration)?.definition.cwd,
        Some(fixture.directory.join("workers"))
    );
    Ok(())
}

#[test]
fn existing_command_path_with_spaces_remains_a_single_token() -> Result<()> {
    let fixture = Fixture::new()?;
    let executable = fixture.directory.join("server with spaces.exe");
    std::fs::write(&executable, "")?;
    let configuration = parse_with_environment(
        &fixture.directory.join("mcporter.json"),
        &serde_json::json!({
            "imports": [],
            "mcpServers": {"test": {"command": executable}}
        })
        .to_string(),
        &|_| Ok(None),
        &|| Ok(BTreeMap::new()),
    )?;
    assert!(server(&configuration)?.definition.args.is_empty());
    assert_eq!(
        PathBuf::from(&server(&configuration)?.definition.command),
        executable
    );
    assert_eq!(
        values::command_tokens(r"node escaped\ argument")?,
        ["node", "escaped argument"]
    );
    assert_eq!(
        values::command_tokens(r"\\host\share\server.exe")?,
        [r"\\host\share\server.exe"]
    );
    Ok(())
}

#[test]
fn stdio_captures_caller_environment_and_applies_explicit_overrides() -> Result<()> {
    #[cfg(windows)]
    let override_name = "Synthetic_Override";
    #[cfg(not(windows))]
    let override_name = "SYNTHETIC_OVERRIDE";
    let configuration = parse_with_environment(
        &source()?,
        r#"{"imports":[],"mcpServers":{"test":{"command":["node","server.js"],"env":{"SYNTHETIC_OVERRIDE":"configured"}}}}"#,
        &|_| Ok(None),
        &|| {
            values::environment_from_os([
                ("SYNTHETIC_CALLER".into(), "forwarded".into()),
                (override_name.into(), "caller".into()),
            ])
        },
    )?;
    let definition = &server(&configuration)?.definition;
    assert!(definition.clear_env);
    assert_eq!(definition.env.len(), 2);
    assert_eq!(
        definition.env.get("SYNTHETIC_CALLER").map(String::as_str),
        Some("forwarded")
    );
    assert_eq!(
        definition.env.get("SYNTHETIC_OVERRIDE").map(String::as_str),
        Some("configured")
    );
    assert_eq!(definition.command, "node");
    assert_eq!(definition.args, ["server.js"]);
    Ok(())
}

#[test]
fn remote_http_does_not_capture_caller_environment() -> Result<()> {
    let configuration = parse_with_environment(
        &source()?,
        r#"{"imports":[],"mcpServers":{"test":{"url":"https://example.invalid/mcp"}}}"#,
        &|_| Ok(None),
        &|| bail!("caller environment must not be captured for HTTP"),
    )?;
    let definition = &server(&configuration)?.definition;
    assert!(!definition.clear_env);
    assert!(definition.env.is_empty());
    Ok(())
}

#[test]
fn non_unicode_caller_environment_fails_without_exposing_values() -> Result<()> {
    #[cfg(unix)]
    let invalid = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![0xff])
    };
    #[cfg(windows)]
    let invalid = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0xd800])
    };
    for entry in [
        ("SYNTHETIC_PRIVATE".into(), invalid.clone()),
        (invalid, "synthetic-private-value".into()),
    ] {
        let error = values::environment_from_os([entry])
            .err()
            .ok_or_else(|| anyhow!("non-Unicode environment unexpectedly accepted"))?;
        let message = error.to_string();
        assert!(message.contains("non-UTF-8"));
        assert!(!message.contains("SYNTHETIC_PRIVATE"));
        assert!(!message.contains("synthetic-private-value"));
    }
    Ok(())
}

#[test]
fn configuration_entry_names_and_sources_are_preserved_for_http_and_stdio() -> Result<()> {
    let configuration = parse(
        r#"{"imports":[],"mcpServers":{
            "stdio-display":{"command":["node"]},
            "http-display":{"url":"https://example.invalid/mcp"}
        }}"#,
    )?;
    for name in ["stdio-display", "http-display"] {
        let entry = configuration
            .servers
            .get(name)
            .ok_or_else(|| anyhow!("synthetic configured server missing"))?;
        assert_eq!(entry.name, name);
        assert_eq!(
            entry.definition.configuration_entry.as_ref(),
            Some(&ConfigurationEntry {
                source: source()?,
                name: name.to_owned(),
            })
        );
        let identity = entry
            .definition
            .configuration_entry
            .as_ref()
            .ok_or_else(|| anyhow!("synthetic configuration entry identity missing"))?;
        assert_eq!(identity.source, entry.source);
    }
    Ok(())
}
