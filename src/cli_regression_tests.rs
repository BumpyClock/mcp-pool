use super::*;

fn configuration() -> Result<ServerConfiguration> {
    crate::server_config::parse_config(
        &PathBuf::from("synthetic.json"),
        r#"{"imports":[],"mcpServers":{"docs":{"command":"echo","env":{"SECRET":"private"}}}}"#,
    )
}

#[test]
fn identities_ignore_ambient_environment_but_track_explicit_resolved_values() -> Result<()> {
    let configuration = configuration()?;
    let selected = configuration.servers.get("docs").context("docs")?;
    let original = pool_name(selected, &selected.definition)?;
    let mut definition = selected.definition.clone();
    definition.env.insert(
        "UNDECLARED_AMBIENT".to_owned(),
        "different-session".to_owned(),
    );
    assert_eq!(original, pool_name(selected, &definition)?);
    assert_eq!(
        definition.env.get("UNDECLARED_AMBIENT").map(String::as_str),
        Some("different-session")
    );
    definition
        .env
        .insert("SECRET".to_owned(), "changed-explicit".to_owned());
    assert_ne!(original, pool_name(selected, &definition)?);
    Ok(())
}

#[test]
fn ad_hoc_tokens_and_final_metadata_reach_the_shared_normalizer() -> Result<()> {
    let configuration = configuration()?;
    let selected = server(
        &configuration,
        "hint",
        &AdHoc {
            command: Some("node \"server script.js\"".to_owned()),
            arguments: vec!["--extra".to_owned(), "value".to_owned()],
            name: Some("final-name".to_owned()),
            persist: Some(
                PathBuf::from("target")
                    .join("other-directory")
                    .join("persisted.json"),
            ),
            ..AdHoc::default()
        },
    )?;
    assert_eq!(selected.definition.command, "node");
    assert_eq!(
        selected.definition.args,
        vec!["server script.js", "--extra", "value"]
    );
    assert_eq!(selected.definition.cwd, Some(std::env::current_dir()?));
    assert_eq!(
        selected.definition.configuration_entry,
        Some(crate::config::ConfigurationEntry {
            source: selected.source.clone(),
            name: "final-name".to_owned()
        })
    );
    assert!(selected.source.ends_with("persisted.json"));
    Ok(())
}

#[test]
fn bare_url_auth_preserves_modifiers_and_uses_non_secret_metadata() -> Result<()> {
    let configuration = configuration()?;
    let selected = server(
        &configuration,
        "http://example.test/mcp?token=private",
        &AdHoc {
            allow_http: true,
            name: Some("logical".to_owned()),
            transport: Some("sse".to_owned()),
            headers: std::collections::BTreeMap::from([(
                "X-Fixture".to_owned(),
                "value".to_owned(),
            )]),
            environment: std::collections::BTreeMap::from([(
                "DECLARED".to_owned(),
                "explicit".to_owned(),
            )]),
            description: Some("description".to_owned()),
            ..AdHoc::default()
        },
    )?;
    assert_eq!(selected.name, "logical");
    assert_eq!(selected.definition.transport, "sse");
    assert_eq!(
        selected
            .definition
            .headers
            .get("X-Fixture")
            .map(String::as_str),
        Some("value")
    );
    assert_eq!(
        selected.definition.env.get("DECLARED").map(String::as_str),
        Some("explicit")
    );
    assert_eq!(selected.definition.description, "description");
    let metadata = serde_json::to_string(&selected.definition.configuration_entry)?;
    assert!(!metadata.contains("private") && !metadata.contains("http://"));
    assert!(
        server(
            &configuration,
            "https://example.test/mcp",
            &AdHoc {
                name: Some("https://example.test/?token=private".to_owned()),
                ..AdHoc::default()
            }
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn single_tool_inference_uses_actual_discovery_and_rejects_ambiguity() -> Result<()> {
    let configuration = configuration()?;
    let selected = configuration.servers.get("docs").context("docs")?;
    assert_eq!(
        invocation::select_tool(selected, &[json!({"name":"echo"})], None)?,
        "echo"
    );
    assert!(invocation::select_tool(selected, &[], None).is_err());
    assert!(
        invocation::select_tool(
            selected,
            &[json!({"name":"one"}), json!({"name":"two"})],
            None
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn tool_filter_aliases_apply_to_calls_discovery_and_single_tool_inference() -> Result<()> {
    let configuration = configuration()?;
    let mut selected = configuration.servers.get("docs").context("docs")?.clone();
    for raw in [
        json!({"allowed_tools":["lookup"]}),
        json!({"blocked_tools":["delete"]}),
        json!({"allowedTools":["lookup"],"allowed_tools":["delete"]}),
    ] {
        selected.raw = raw;
        assert!(tool_allowed(&selected, "lookup")?);
        assert!(!tool_allowed(&selected, "delete")?);
        assert_eq!(
            invocation::select_tool(
                &selected,
                &[json!({"name":"lookup"}), json!({"name":"delete"})],
                None
            )?,
            "lookup"
        );
    }
    for raw in [
        json!({"allowed_tools":"lookup"}),
        json!({"blocked_tools":[1]}),
        json!({"allowedTools":["lookup"],"blocked_tools":["delete"]}),
    ] {
        selected.raw = raw;
        assert!(tool_allowed(&selected, "delete").is_err());
    }
    Ok(())
}
