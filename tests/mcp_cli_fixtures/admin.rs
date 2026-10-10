use std::io;

use serde_json::{Value, json};

use super::support::{Fixture, parse_json};

#[tokio::test]
async fn ad_hoc_persistence_rejects_saved_names_without_changing_file_or_pool() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    let remote = super::http::HttpFixture::new().await?;
    let original = tokio::fs::read(&fixture.config).await?;
    let config_path = fixture
        .config
        .to_str()
        .ok_or_else(|| io::Error::other("non-Unicode config path"))?;
    let rejected = fixture
        .command(&[
            "list",
            "--http-url",
            &remote.url,
            "--allow-http",
            "--name",
            "fixture",
            "--persist",
            config_path,
            "--json",
        ])
        .await?;
    assert!(!rejected.status.success(), "{rejected:?}");
    let error = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        error.contains("already exists") && error.contains("config add"),
        "{error}"
    );
    assert_eq!(tokio::fs::read(&fixture.config).await?, original);
    assert!(remote.headers.lock().await.is_empty());
    fixture
        .success(&[
            "call",
            "fixture.echo",
            "label=unchanged",
            "--output",
            "json",
        ])
        .await?;
    assert_eq!(fixture.event_count("spawn").await?, 1);

    let saved = parse_json(
        &fixture
            .success(&[
                "list",
                "--http-url",
                &remote.url,
                "--allow-http",
                "--name",
                "added",
                "--persist",
                config_path,
                "--json",
            ])
            .await?,
    )?;
    assert_eq!(saved.get("status"), Some(&json!("ok")));
    let updated: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    let previous: Value = serde_json::from_slice(&original).map_err(io::Error::other)?;
    assert_eq!(
        updated.pointer("/mcpServers/fixture"),
        previous.pointer("/mcpServers/fixture")
    );
    assert_eq!(
        updated.pointer("/mcpServers/array"),
        previous.pointer("/mcpServers/array")
    );
    assert_eq!(
        updated.pointer("/mcpServers/added/baseUrl"),
        Some(&json!(remote.url))
    );
    fixture.finish().await
}

#[tokio::test]
async fn list_aliases_status_and_explicit_config_select_synthetic_servers() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    fixture.warm("array").await?;
    for command in ["describe", "list-tools"] {
        let response = parse_json(
            &fixture
                .success(&[command, "fixture.echo", "--json"])
                .await?,
        )?;
        assert_eq!(response.get("name"), Some(&json!("fixture")));
        let tools = response
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| io::Error::other("alias omitted tools"))?;
        assert_eq!(tools.len(), 1);
        assert_eq!(response.pointer("/tools/0/name"), Some(&json!("echo")));
    }
    let listed = parse_json(&fixture.success(&["list", "--json", "--status"]).await?)?;
    assert_eq!(listed.get("mode"), Some(&json!("list")));
    assert_eq!(listed.pointer("/counts/ok"), Some(&json!(2)));
    let servers = listed
        .get("servers")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("summary omitted servers"))?;
    let names: Vec<_> = servers
        .iter()
        .filter_map(|server| server.get("name").and_then(Value::as_str))
        .collect();
    assert_eq!(names, ["array", "fixture"]);
    let alternate = fixture.home.join("alternate.json");
    tokio::fs::write(&alternate, "{\"imports\":[],\"mcpServers\":{}}").await?;
    let alternate_path = alternate
        .to_str()
        .ok_or_else(|| io::Error::other("non-Unicode config path"))?;
    let empty = parse_json(
        &fixture
            .success(&["--config", alternate_path, "list", "--json"])
            .await?,
    )?;
    assert_eq!(empty.pointer("/counts/ok"), Some(&json!(0)));
    assert_eq!(empty.get("servers"), Some(&json!([])));
    let invalid = fixture
        .command(&["list", "fixture", "--brief", "--json"])
        .await?;
    assert!(!invalid.status.success());
    fixture.finish().await
}

#[tokio::test]
async fn vault_tokens_auth_reuse_and_clear_are_isolated_and_redacted() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let remote = super::http::HttpFixture::new().await?;
    let configuration = json!({"imports":[], "mcpServers":{"protected":{
        "baseUrl":remote.url, "auth":"oauth",
        "env":{"HOME":fixture.home,"USERPROFILE":fixture.home,"XDG_DATA_HOME":fixture.home}
    }}});
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    let payload = json!({"tokens":{
        "access_token":"synthetic-access-token", "token_type":"Bearer", "expires_at":4102444800u64
    }, "clientInfo":{"client_id":"synthetic-client"}});
    let tokens = fixture.home.join("synthetic-tokens.json");
    tokio::fs::write(&tokens, payload.to_string()).await?;
    let tokens_path = tokens
        .to_str()
        .ok_or_else(|| io::Error::other("non-Unicode token path"))?;
    let written = fixture
        .success(&["vault", "set", "protected", "--tokens-file", tokens_path])
        .await?;
    assert!(!String::from_utf8_lossy(&written.stdout).contains("synthetic-access-token"));
    let authenticated = parse_json(
        &fixture
            .success(&["auth", "protected", "--no-browser", "--json"])
            .await?,
    )?;
    assert_eq!(authenticated.get("status"), Some(&json!("authenticated")));
    assert_eq!(authenticated.get("reused"), Some(&json!(true)));
    assert!(!authenticated.to_string().contains("synthetic-access-token"));
    let doctor = parse_json(&fixture.success(&["config", "doctor", "--json"]).await?)?;
    assert_eq!(
        doctor.pointer("/servers/0/credentials/authenticated"),
        Some(&json!(true))
    );
    assert!(!doctor.to_string().contains("synthetic-access-token"));
    assert!(!doctor.to_string().contains("synthetic-client"));
    let response = parse_json(
        &fixture
            .success(&["call", "protected.headers", "--output", "json"])
            .await?,
    )?;
    assert_eq!(
        response.pointer("/headers/authorization"),
        Some(&json!("Bearer synthetic-access-token"))
    );
    fixture.success(&["vault", "clear", "protected"]).await?;
    let doctor = parse_json(&fixture.success(&["config", "doctor", "--json"]).await?)?;
    assert_eq!(
        doctor.pointer("/servers/0/credentials/hasTokens"),
        Some(&json!(false))
    );
    let missing = fixture
        .command(&["call", "protected.headers", "--output", "json"])
        .await?;
    assert!(!missing.status.success());
    assert!(!String::from_utf8_lossy(&missing.stdout).contains("synthetic-access-token"));
    let stdin = fixture
        .command_input(
            &["vault", "set", "protected", "--stdin"],
            Some(&payload.to_string()),
        )
        .await?;
    assert!(stdin.status.success(), "{stdin:?}");
    assert!(
        fixture
            .home
            .join("mcporter")
            .join("credentials.json")
            .exists()
    );
    fixture.success(&["config", "logout", "protected"]).await?;
    fixture.finish().await
}

#[tokio::test]
async fn daemon_administration_and_native_namespace_keep_their_meanings() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    let status = fixture.success(&["daemon", "status", "--json"]).await?;
    assert_eq!(parse_json(&status)?.get("running"), Some(&json!(true)));
    let native = parse_json(&fixture.success(&["pool", "list", "--json"]).await?)?;
    assert_eq!(native, json!([]));
    let global_native = parse_json(&fixture.success(&["--json", "pool", "list"]).await?)?;
    assert_eq!(global_native, json!([]));
    assert!(!fixture.counter.exists());
    fixture.success(&["daemon", "start"]).await?;
    fixture.success(&["daemon", "restart"]).await?;
    fixture.success(&["daemon", "status", "--json"]).await?;
    let deferred = parse_json(&fixture.success(&["daemon", "migrate"]).await?)?;
    assert_eq!(deferred, json!([]));
    let forbidden = fixture
        .command(&["daemon", "migrate", "--stop-legacy"])
        .await?;
    assert!(!forbidden.status.success());
    let import = fixture.command(&["config", "import"]).await?;
    assert!(!import.status.success());
    assert!(!fixture.counter.exists());
    fixture.success(&["daemon", "stop"]).await?;
    assert_eq!(
        parse_json(&fixture.success(&["daemon", "status", "--json"]).await?)?,
        json!({"running":false})
    );
    let started = parse_json(&fixture.success(&["daemon", "start", "--json"]).await?)?;
    assert_eq!(started.get("running"), Some(&json!(true)));
    fixture.finish().await
}

#[tokio::test]
async fn discovery_reports_auth_failure_and_quiet_health_exit_code() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.warm("fixture").await?;
    fixture.warm("array").await?;
    let mut configuration: Value = serde_json::from_slice(&tokio::fs::read(&fixture.config).await?)
        .map_err(io::Error::other)?;
    let servers = configuration
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| io::Error::other("config omitted servers"))?;
    servers.insert(
        "locked".into(),
        json!({
            "baseUrl":"http://127.0.0.1:1/mcp", "auth":"oauth",
            "env":{"HOME":fixture.home,"USERPROFILE":fixture.home,"XDG_DATA_HOME":fixture.home}
        }),
    );
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    let response = parse_json(&fixture.success(&["list", "--json"]).await?)?;
    assert_eq!(response.pointer("/counts/ok"), Some(&json!(2)));
    assert_eq!(response.pointer("/counts/auth"), Some(&json!(1)));
    assert_eq!(response.pointer("/servers/2/name"), Some(&json!("locked")));
    assert_eq!(response.pointer("/servers/2/status"), Some(&json!("auth")));
    assert_eq!(
        response.pointer("/servers/2/authCommand"),
        Some(&json!("mcp-pool auth locked"))
    );
    let quiet = fixture.command(&["list", "--quiet"]).await?;
    assert!(!quiet.status.success());
    assert!(quiet.stdout.is_empty());
    let strict = fixture.command(&["list", "--json", "--exit-code"]).await?;
    assert!(!strict.status.success());
    assert_eq!(
        parse_json(&strict)?.pointer("/counts/auth"),
        Some(&json!(1))
    );
    fixture.finish().await
}

#[tokio::test]
async fn caller_read_only_policy_reaches_existing_daemon_without_refresh_or_writes()
-> io::Result<()> {
    let fixture = Fixture::new().await?;
    let remote = super::http::HttpFixture::new().await?;
    let origin = remote
        .url
        .strip_suffix("/mcp")
        .ok_or_else(|| io::Error::other("mock URL omitted mcp path"))?;
    let configuration = json!({"imports":[], "mcpServers":{"guarded":{
        "baseUrl":remote.url,"auth":"oauth","timeoutMs":1000,
        "env":{"HOME":fixture.home,"USERPROFILE":fixture.home,"XDG_DATA_HOME":fixture.home}
    }}});
    tokio::fs::write(&fixture.config, configuration.to_string()).await?;
    let payload = json!({
        "tokens":{"access_token":"synthetic-old","refresh_token":"synthetic-refresh",
            "token_type":"Bearer","expires_at":1,"issuer":origin},
        "clientInfo":{"client_id":"synthetic-client","token_endpoint_auth_method":"none","issuer":origin},
        "authorizationServerUrl":origin,"resourceUrl":remote.url,
        "discoveryState":{
            "authorizationServerUrl":origin,
            "resourceMetadata":{"resource":remote.url,"authorization_servers":[origin]},
            "authorizationServerMetadata":{"issuer":origin,"authorization_endpoint":format!("{origin}/authorize"),
                "token_endpoint":format!("{origin}/token"),"code_challenge_methods_supported":["S256"],
                "token_endpoint_auth_methods_supported":["none"]}
        }
    });
    let seeded = fixture
        .command_input(
            &["vault", "set", "guarded", "--stdin"],
            Some(&payload.to_string()),
        )
        .await?;
    assert!(seeded.status.success(), "{seeded:?}");
    let vault = fixture.home.join("mcporter").join("credentials.json");
    let snapshot = tokio::fs::read(&vault).await?;
    assert_eq!(
        parse_json(&fixture.success(&["daemon", "status", "--json"]).await?)?.get("running"),
        Some(&json!(true))
    );
    for arguments in [
        vec!["list", "guarded", "--json"],
        vec!["call", "guarded.headers", "--output", "json"],
    ] {
        let output = fixture
            .command_input_environment(&arguments, None, &[("MCP_POOL_CREDENTIALS_READ_ONLY", "1")])
            .await?;
        assert!(!output.status.success(), "{output:?}");
        assert_eq!(tokio::fs::read(&vault).await?, snapshot);
        assert!(
            remote.headers.lock().await.is_empty(),
            "guarded caller sent an HTTP request"
        );
        let report = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(report.contains("read-only"), "{report}");
        assert!(!report.contains("synthetic-refresh"), "{report}");
    }
    assert_eq!(
        parse_json(&fixture.success(&["daemon", "status", "--json"]).await?)?.get("running"),
        Some(&json!(true))
    );
    fixture.finish().await
}

#[tokio::test]
async fn daemon_relative_log_is_bounded_and_filters_logical_server_names() -> io::Result<()> {
    let fixture = Fixture::new().await?;
    fixture.success(&["daemon", "stop"]).await?;
    let log_file = fixture.home.join("selected-daemon.log");
    tokio::fs::write(&log_file, "synthetic older log\n".repeat(300_000)).await?;
    assert!(tokio::fs::metadata(&log_file).await?.len() > 5 * 1024 * 1024);
    let project_directory = std::env::current_dir()?.canonicalize()?;
    let relative_log_file = log_file
        .strip_prefix(&project_directory)
        .map_err(io::Error::other)?;
    assert!(!relative_log_file.is_absolute());
    let relative_log_file = relative_log_file
        .to_str()
        .ok_or_else(|| io::Error::other("non-Unicode relative log path"))?;
    let started = parse_json(
        &fixture
            .success(&[
                "daemon",
                "start",
                "--log",
                "--log-file",
                relative_log_file,
                "--log-servers",
                "fixture",
                "--json",
            ])
            .await?,
    )?;
    assert_eq!(started.get("running"), Some(&json!(true)));

    fixture.warm("fixture").await?;
    let first_status = parse_json(&fixture.success(&["pool", "status", "--json"]).await?)?;
    let first_servers = first_status
        .get("servers")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("native status omitted servers"))?;
    assert_eq!(first_servers.len(), 1);
    let selected = first_servers
        .first()
        .and_then(|server| server.get("name"))
        .and_then(Value::as_str)
        .ok_or_else(|| io::Error::other("native status omitted selected name"))?
        .to_owned();
    let selected_call = parse_json(
        &fixture
            .success(&[
                "call",
                "fixture.echo",
                "label=logging-selected",
                "--output",
                "json",
            ])
            .await?,
    )?;
    assert_eq!(
        selected_call.pointer("/arguments/label"),
        Some(&json!("logging-selected"))
    );

    fixture.warm("array").await?;
    let excluded_call = parse_json(
        &fixture
            .success(&[
                "call",
                "array.echo",
                "label=logging-excluded",
                "--output",
                "json",
            ])
            .await?,
    )?;
    assert_eq!(
        excluded_call.pointer("/arguments/label"),
        Some(&json!("logging-excluded"))
    );
    let second_status = parse_json(&fixture.success(&["pool", "status", "--json"]).await?)?;
    let second_servers = second_status
        .get("servers")
        .and_then(Value::as_array)
        .ok_or_else(|| io::Error::other("native status omitted servers"))?;
    assert_eq!(second_servers.len(), 2);
    let excluded = second_servers
        .iter()
        .filter_map(|server| server.get("name").and_then(Value::as_str))
        .find(|name| *name != selected)
        .ok_or_else(|| io::Error::other("native status omitted excluded name"))?;
    assert_eq!(fixture.event_count("spawn").await?, 2);
    assert_eq!(fixture.event_count("tools/call").await?, 2);

    let length = tokio::fs::metadata(&log_file).await?.len();
    assert!(
        length <= 5 * 1024 * 1024,
        "daemon log exceeded 5 MiB: {length}"
    );
    let contents = tokio::fs::read_to_string(&log_file).await?;
    assert!(
        contents.contains("daemon starting"),
        "daemon startup was not logged"
    );
    assert!(
        contents.contains(&format!(
            "pool_proxy_starting name={selected} transport=stdio"
        )),
        "selected server startup was not retained"
    );
    let excluded_field = format!("name={excluded}");
    assert!(
        !contents
            .lines()
            .any(|line| line.split_whitespace().any(|field| field == excluded_field)),
        "excluded server's identifiable logs were retained"
    );
    fixture.finish().await
}
