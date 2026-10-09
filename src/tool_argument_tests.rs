use super::*;
use serde_json::json;

fn parse(tokens: &[&str]) -> Result<Call> {
    call(tokens.iter().map(|token| (*token).to_owned()).collect())
}

#[test]
fn dotted_separate_and_expression_calls() -> Result<()> {
    for tokens in [
        vec!["docs.search", "query=hello", "limit:3"],
        vec!["docs", "search", "query:", "hello", "limit=3"],
        vec!["docs.search(query: 'hello', limit: 3)"],
        vec![
            "--server",
            "docs",
            "--tool",
            "search",
            "query=hello",
            "limit=3",
        ],
    ] {
        let parsed = parse(&tokens)?;
        assert_eq!(parsed.server.as_deref(), Some("docs"));
        assert_eq!(parsed.tool.as_deref(), Some("search"));
        assert_eq!(
            parsed.arguments,
            json!({"query":"hello","limit":3})
                .as_object()
                .cloned()
                .context("object")?
        );
    }

    Ok(())
}

#[test]
fn dotted_tool_names_keep_the_full_suffix_in_selectors_and_expressions() -> Result<()> {
    for tokens in [
        vec!["docs.search.v2", "query=fixture"],
        vec!["docs.search.v2(query: 'fixture')"],
    ] {
        let parsed = parse(&tokens)?;
        assert_eq!(parsed.server.as_deref(), Some("docs"));
        assert_eq!(parsed.tool.as_deref(), Some("search.v2"));
        assert_eq!(parsed.arguments.get("query"), Some(&json!("fixture")));
    }
    Ok(())
}

#[test]
fn output_flags_do_not_steal_named_tool_arguments() -> Result<()> {
    let parsed = parse(&[
        "docs.search",
        "--format",
        "json",
        "--output",
        "raw",
        "--json",
        "{\"limit\":4}",
    ])?;
    assert_eq!(parsed.arguments.get("format"), Some(&json!("json")));
    assert_eq!(parsed.arguments.get("limit"), Some(&json!(4)));
    assert_eq!(parsed.output, Output::Raw);
    Ok(())
}

#[test]
fn flags_nested_expression_literal_and_raw_strings() -> Result<()> {
    let parsed = parse(&[
        "--server",
        "docs",
        "--tool",
        "search",
        "--raw-strings",
        "--limit",
        "001",
    ])?;
    assert_eq!(parsed.arguments.get("limit"), Some(&json!("001")));
    let parsed = parse(&[
        "docs.search(filter: {\"items\":[1,2]}, query: 'a,b')",
        "--",
        "--output",
    ])?;
    assert_eq!(
        parsed.arguments.get("filter"),
        Some(&json!({"items":[1,2]}))
    );
    assert_eq!(parsed.positionals, vec![json!("--output")]);
    assert!(parse(&["docs.search", "--args", "[]"]).is_err());
    assert!(parse(&["docs.search", "--timeout", "0"]).is_err());
    assert!(parse(&["docs.search(filter: [})"]).is_err());
    Ok(())
}

#[test]
fn schema_hydrates_positionals_and_preserves_string_numbers() -> Result<()> {
    let mut parsed = parse(&["docs.search", "hello", "limit=2", "identifier=001"])?;
    hydrate(
        &mut parsed,
        &json!({"type":"object","required":["query"],"properties":{
            "query":{"type":"string"},"limit":{"type":"integer"},"identifier":{"type":"string"}
        }}),
    )?;
    assert_eq!(parsed.arguments.get("query"), Some(&json!("hello")));
    assert_eq!(parsed.arguments.get("identifier"), Some(&json!("001")));
    Ok(())
}

#[test]
fn numeric_string_modes_preserve_reference_coercion_boundaries() -> Result<()> {
    let parsed = parse(&[
        "docs.search",
        "--raw-strings",
        "count=3",
        "enabled=true",
        "missing=none",
        "items=[1,2]",
    ])?;
    assert_eq!(parsed.arguments.get("count"), Some(&json!("3")));
    assert_eq!(parsed.arguments.get("enabled"), Some(&json!(true)));
    assert_eq!(parsed.arguments.get("missing"), Some(&Value::Null));
    assert_eq!(parsed.arguments.get("items"), Some(&json!([1, 2])));
    let parsed = parse(&[
        "docs.search",
        "--no-coerce",
        "enabled=true",
        "missing=null",
        "items=[1,2]",
    ])?;
    assert_eq!(parsed.arguments.get("enabled"), Some(&json!("true")));
    assert_eq!(parsed.arguments.get("missing"), Some(&json!("null")));
    assert_eq!(parsed.arguments.get("items"), Some(&json!("[1,2]")));
    assert_eq!(coerce("1.0", false), json!("1.0"));
    assert_eq!(coerce("-0", false), json!("-0"));
    assert_eq!(coerce("1e-7", false), json!(0.0000001));
    assert_eq!(coerce("1e+21", false), json!(1e21));
    Ok(())
}

#[test]
fn ad_hoc_http_selectors_do_not_split_hostname_as_tool() -> Result<()> {
    let parsed = parse(&["https://example.test/mcp", "search", "query=x"])?;
    assert_eq!(
        parsed.ephemeral.url.as_deref(),
        Some("https://example.test/mcp")
    );
    assert_eq!(parsed.tool.as_deref(), Some("search"));
    let parsed = parse(&["https://example.test/mcp.search", "query=x"])?;
    assert_eq!(
        parsed.ephemeral.url.as_deref(),
        Some("https://example.test/mcp")
    );
    assert_eq!(parsed.tool.as_deref(), Some("search"));
    Ok(())
}

#[test]
fn ad_hoc_stdio_commands_can_be_explicit_or_inferred() -> Result<()> {
    for tokens in [
        vec!["--stdio", "npx -y fixture", "search", "query=x"],
        vec!["npx -y fixture", "search", "query=x"],
        vec![r"C:\tools\fixture.exe", "search", "query=x"],
    ] {
        let parsed = parse(&tokens)?;
        assert!(parsed.ephemeral.command.is_some());
        assert_eq!(parsed.tool.as_deref(), Some("search"));
    }
    Ok(())
}

#[test]
fn schema_validates_enums_unknown_keys_and_camel_case_flags() -> Result<()> {
    let mut parsed = parse(&["docs.search", "--page-size", "3", "mode=fast"])?;
    hydrate(
        &mut parsed,
        &json!({"type":"object","additionalProperties":false,
        "properties":{"pageSize":{"type":"integer"},"mode":{"type":"string","enum":["fast","slow"]}}}),
    )?;
    assert_eq!(parsed.arguments.get("pageSize"), Some(&json!(3)));
    let mut bad = parse(&["docs.search", "extra=3"])?;
    assert!(
        hydrate(
            &mut bad,
            &json!({"additionalProperties":false,"properties":{}})
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn positional_order_is_schema_declaration_order_not_required_order() -> Result<()> {
    let mut parsed = parse(&["docs.search", "first", "second"])?;
    let schema: Value = serde_json::from_str(
        r#"{"required":["alpha","zulu"],"properties":{"zulu":{"type":"string"},"alpha":{"type":"string"}}}"#,
    )?;
    hydrate(&mut parsed, &schema)?;
    assert_eq!(parsed.arguments.get("zulu"), Some(&json!("first")));
    assert_eq!(parsed.arguments.get("alpha"), Some(&json!("second")));
    assert_eq!(
        crate::tool_output::options(&json!({"inputSchema":schema}))
            .first()
            .and_then(|option| option.get("property")),
        Some(&json!("zulu"))
    );
    Ok(())
}

#[test]
fn unknown_generic_flags_are_rejected_without_additional_properties_restriction() -> Result<()> {
    let mut parsed = parse(&["docs.search", "--timout", "3"])?;
    assert!(
        hydrate(
            &mut parsed,
            &json!({"properties":{"timeout":{"type":"integer"}}})
        )
        .is_err()
    );
    let mut named = parse(&["docs.search", "extra=3"])?;
    hydrate(&mut named, &json!({"properties":{}}))?;
    assert_eq!(named.arguments.get("extra"), Some(&json!(3)));
    let mut acronym = parse(&["docs.search", "--api-key", "literal"])?;
    hydrate(
        &mut acronym,
        &json!({"properties":{"APIKey":{"type":"string"}}}),
    )?;
    assert_eq!(acronym.arguments.get("APIKey"), Some(&json!("literal")));
    assert_eq!(crate::tool_output::kebab("HTTPResponse"), "http-response");
    Ok(())
}

#[test]
fn http_tool_queries_fragments_and_query_parentheses_are_preserved() -> Result<()> {
    let parsed = parse(&[
        "https://example.test/mcp.search?tenant=acme#fragment",
        "query=x",
    ])?;
    assert_eq!(
        parsed.ephemeral.url.as_deref(),
        Some("https://example.test/mcp?tenant=acme")
    );
    assert_eq!(parsed.tool.as_deref(), Some("search"));
    let parsed = parse(&["https://example.test/mcp.search?filter=(x)", "query=x"])?;
    assert_eq!(
        parsed.ephemeral.url.as_deref(),
        Some("https://example.test/mcp?filter=(x)")
    );
    assert_eq!(parsed.tool.as_deref(), Some("search"));
    Ok(())
}

#[test]
fn named_file_expansion_is_literal_and_does_not_touch_positionals_or_json() -> Result<()> {
    let directory = std::env::current_dir()?
        .join("target")
        .join(format!("named-file-fixture-{}", std::process::id()));
    std::fs::create_dir_all(&directory)?;
    let path = directory.join("input.txt");
    std::fs::write(&path, "{\"quoted\":true}\n")?;
    let result = (|| {
        let file = format!("@{}", path.display());
        let label = format!("label={file}");
        let mut parsed = parse(&["docs.search", &label, "literal=@@literal"])?;
        hydrate(
            &mut parsed,
            &json!({"properties":{"label":{"type":"string"},"literal":{"type":"string"}}}),
        )?;
        assert_eq!(
            parsed.arguments.get("label"),
            Some(&json!("{\"quoted\":true}\n"))
        );
        assert_eq!(parsed.arguments.get("literal"), Some(&json!("@literal")));
        let parsed = parse(&[
            "docs.search",
            "--label",
            &file,
            "@@positional",
            "--args",
            r#"{"literal":"@not-expanded"}"#,
        ])?;
        assert_eq!(
            parsed.arguments.get("label"),
            Some(&json!("{\"quoted\":true}\n"))
        );
        assert_eq!(
            parsed.arguments.get("literal"),
            Some(&json!("@not-expanded"))
        );
        assert_eq!(parsed.positionals, vec![json!("@@positional")]);
        assert!(parse(&["docs.search", "label=@"]).is_err());
        std::fs::write(&path, [255u8])?;
        assert!(parse(&["docs.search", &label]).is_err());
        Ok(())
    })();
    std::fs::remove_dir_all(directory)?;
    result
}

#[test]
fn ad_hoc_and_artifact_workflow_flags_have_real_parser_fields() -> Result<()> {
    let parsed = parse(&[
        "--stdio",
        "node server.js",
        "--stdio-arg",
        "extra",
        "--name",
        "example",
        "--persist",
        "fixture.json",
        "--description",
        "fixture",
        "echo",
        "--save-images",
        "images",
        "--tail-log",
    ])?;
    assert_eq!(parsed.ephemeral.arguments, vec!["extra"]);
    assert_eq!(
        parsed.ephemeral.persist,
        Some(PathBuf::from("fixture.json"))
    );
    assert_eq!(parsed.ephemeral.description.as_deref(), Some("fixture"));
    assert_eq!(parsed.save_images, Some(PathBuf::from("images")));
    assert!(parsed.tail_log);
    Ok(())
}

#[test]
fn aliases_are_normalized_before_positional_allocation_and_keep_assignment_order() -> Result<()> {
    let schema = json!({"properties":{"pageSize":{"type":"integer"},"query":{"type":"string"}}});
    let mut parsed = parse(&["docs.search", "--page-size", "3", "hello"])?;
    hydrate(&mut parsed, &schema)?;
    assert_eq!(
        parsed.arguments,
        json!({"pageSize":3,"query":"hello"})
            .as_object()
            .cloned()
            .context("object")?
    );
    let mut parsed = parse(&[
        "docs.search",
        "--page-size",
        "3",
        "--args",
        r#"{"pageSize":7}"#,
        "hello",
    ])?;
    hydrate(&mut parsed, &schema)?;
    assert_eq!(parsed.arguments.get("pageSize"), Some(&json!(7)));
    Ok(())
}

#[test]
fn stdin_json_inputs_preserve_chunk_and_flag_assignment_order() -> Result<()> {
    let mut earlier = parse(&[
        "docs.search",
        "--args",
        r#"{"value":"earlier"}"#,
        "--params",
        "-",
    ])?;
    merge_stdin(&mut earlier, r#"{"value":"stdin"}"#)?;
    assert_eq!(earlier.arguments.get("value"), Some(&json!("stdin")));
    let mut later = parse(&[
        "docs.search",
        "--params",
        "-",
        "--args",
        r#"{"value":"later"}"#,
    ])?;
    merge_stdin(&mut later, r#"{"value":"stdin"}"#)?;
    assert_eq!(later.arguments.get("value"), Some(&json!("later")));
    let mut named = parse(&[
        "docs.search",
        "--value",
        "earlier",
        "--args",
        "-",
        "value=trailing",
    ])?;
    merge_stdin(&mut named, r#"{"value":"stdin"}"#)?;
    assert_eq!(named.arguments.get("value"), Some(&json!("trailing")));
    let mut schema = parse(&["docs.search", "--value", "earlier", "--args", "-"])?;
    merge_stdin(&mut schema, r#"{"value":"stdin"}"#)?;
    hydrate(
        &mut schema,
        &json!({"properties":{"value":{"type":"string"}}}),
    )?;
    assert_eq!(schema.arguments.get("value"), Some(&json!("stdin")));
    Ok(())
}

#[test]
fn ad_hoc_function_calls_and_single_tool_inference_reach_discovery() -> Result<()> {
    let parsed = parse(&["--stdio", "node fixture.js", "echo(query: 'hello')"])?;
    assert_eq!(parsed.tool.as_deref(), Some("echo"));
    assert_eq!(parsed.arguments.get("query"), Some(&json!("hello")));
    let parsed = parse(&["docs", "query=hello"])?;
    assert_eq!(parsed.server.as_deref(), Some("docs"));
    assert_eq!(parsed.tool, None);
    assert_eq!(parsed.arguments.get("query"), Some(&json!("hello")));
    Ok(())
}
