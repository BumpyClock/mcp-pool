use super::*;
use crate::tool_arguments::{call, merge_stdin};
use serde_json::json;

fn parse(tokens: &[&str]) -> Result<Call> {
    call(tokens.iter().map(|token| (*token).to_owned()).collect())
}

#[test]
fn inline_and_local_reference_properties_hydrate_identically() -> Result<()> {
    let properties = json!({"identifier":{"type":"string"},"values":{"type":"array"},
        "count":{"type":"integer"},"enabled":{"type":"boolean"}});
    let inline = json!({"properties":properties});
    let referenced = json!({"$defs":properties,"properties":{
        "identifier":{"$ref":"#/$defs/identifier"},"values":{"$ref":"#/$defs/values"},
        "count":{"$ref":"#/$defs/count"},"enabled":{"$ref":"#/$defs/enabled"}}});
    for tokens in [
        vec![
            "docs.search",
            "identifier=123",
            "values=a,b",
            "count=4",
            "enabled=true",
        ],
        vec!["docs.search", "123", "a,b", "4", "true"],
        vec![
            "docs.search",
            "--args",
            r#"{"identifier":123,"values":"a,b","count":4,"enabled":true}"#,
        ],
    ] {
        let mut first = parse(&tokens)?;
        let mut second = parse(&tokens)?;
        hydrate(&mut first, &inline)?;
        hydrate(&mut second, &referenced)?;
        assert_eq!(first.arguments, second.arguments);
        assert_eq!(
            second.arguments,
            json!({"identifier":"123","values":["a","b"],"count":4,"enabled":true})
                .as_object()
                .cloned()
                .context("arguments")?
        );
    }
    for schema in [&inline, &referenced] {
        let mut invalid = parse(&["docs.search", "count=wrong"])?;
        assert!(hydrate(&mut invalid, schema).is_err());
    }
    Ok(())
}

#[test]
fn ambiguous_property_aliases_fail_and_exact_names_remain_usable() -> Result<()> {
    let schema = json!({"properties":{"apiKey":{"type":"string"},"api_key":{"type":"string"}}});
    for tokens in [
        vec!["docs.search", "--api-key", "value"],
        vec!["docs.search", "api-key=value"],
        vec!["docs.search", "--args", r#"{"api-key":"value"}"#],
    ] {
        let mut parsed = parse(&tokens)?;
        let error = hydrate(&mut parsed, &schema)
            .err()
            .context("ambiguous alias must fail")?;
        assert_eq!(
            error.to_string(),
            "Tool option 'api-key' is ambiguous; matches: apiKey, api_key"
        );
    }
    let mut exact = parse(&["docs.search", "--apiKey", "one", "--api_key", "two"])?;
    hydrate(&mut exact, &schema)?;
    assert_eq!(exact.arguments.get("apiKey"), Some(&json!("one")));
    assert_eq!(exact.arguments.get("api_key"), Some(&json!("two")));
    let exact_schema = json!({"properties":{"apiKey":{},"api_key":{},"api-key":{"type":"string"}}});
    let mut exact = parse(&["docs.search", "--api-key", "literal"])?;
    hydrate(&mut exact, &exact_schema)?;
    assert_eq!(exact.arguments.get("api-key"), Some(&json!("literal")));
    Ok(())
}

#[test]
fn ordered_provenance_follows_named_expression_json_and_stdin_assignments() -> Result<()> {
    let schema = json!({"properties":{"pageSize":{"type":"string"}}});
    for (tokens, expected) in [
        (vec!["docs.search", "page-size=123", "--args", "-"], "stdin"),
        (
            vec!["docs.search(page-size: 123)", "--pageSize", "456"],
            "456",
        ),
        (vec!["docs.search", "--args", "-", "page-size=001"], "001"),
        (
            vec![
                "docs.search",
                "--page-size",
                "123",
                "--args",
                r#"{"pageSize":"json"}"#,
            ],
            "json",
        ),
    ] {
        let mut parsed = parse(&tokens)?;
        if parsed.inputs.has_stdin() {
            merge_stdin(&mut parsed, r#"{"pageSize":"stdin"}"#)?;
        }
        hydrate(&mut parsed, &schema)?;
        assert_eq!(parsed.arguments.get("pageSize"), Some(&json!(expected)));
        assert_eq!(parsed.arguments.len(), 1);
    }
    Ok(())
}

#[test]
fn parsed_json_intent_respects_literal_boundaries_and_option_values() -> Result<()> {
    for tokens in [
        vec!["docs.search", "--args", "{}"],
        vec!["docs.search", "--params", "-"],
        vec!["docs.search", "--json", "{}"],
    ] {
        assert!(parse(&tokens)?.inputs.has_explicit_json());
    }
    for tokens in [
        vec!["docs.search", "--", "--args"],
        vec!["docs.search", "--label=--json"],
        vec!["--stdio", "fixture", "--stdio-arg", "--args", "search"],
    ] {
        assert!(!parse(&tokens)?.inputs.has_explicit_json());
    }
    Ok(())
}

#[test]
fn referenced_unions_and_bounded_cycles_share_the_schema_view() -> Result<()> {
    let schema = json!({"$defs":{"nullable":{"type":["string","null"]},"a/b":{"type":"integer"},
        "cycle":{"$ref":"#/$defs/cycle"}},"properties":{"label":{"$ref":"#/$defs/nullable"},
        "count":{"$ref":"#/$defs/a~1b"},"cycle":{"$ref":"#/$defs/cycle"}}});
    let mut parsed = parse(&["docs.search", "label=null", "count=2", "cycle=anything"])?;
    hydrate(&mut parsed, &schema)?;
    assert_eq!(parsed.arguments.get("label"), Some(&Value::Null));
    assert_eq!(parsed.arguments.get("count"), Some(&json!(2)));
    assert_eq!(parsed.arguments.get("cycle"), Some(&json!("anything")));
    let mut parsed = parse(&["docs.search", "count=wrong"])?;
    assert!(hydrate(&mut parsed, &schema).is_err());
    Ok(())
}
