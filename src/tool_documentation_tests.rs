use serde_json::json;

use super::*;

fn plain() -> Style {
    Style {
        color: false,
        width: 100,
    }
}

#[test]
fn documentation_preserves_descriptions_and_complete_parameter_types() {
    let tool = json!({
        "name":"update_shader",
        "description":"Update an existing shader.\nDo not create a new resource.",
        "inputSchema":{"type":"object","required":["id","commitMessage","kind"],"properties":{
            "id":{"type":"string","description":"The resource ID."},
            "files":{"type":"array","items":{"type":"object"},"description":"Existing files to replace."},
            "metadata":{"type":"object"},
            "commitMessage":{"type":"string","description":"Required commit message."},
            "kind":{"type":"string","enum":["effect","fill"],"description":"The matching resource kind."}
        }}
    });
    let output = plain().render(&tool, false);
    assert!(!output.hidden_parameters);
    assert_eq!(
        output.text,
        concat!(
            "  /**\n",
            "   * Update an existing shader.\n",
            "   * Do not create a new resource.\n",
            "   *\n",
            "   * @param id The resource ID.\n",
            "   * @param files? Existing files to replace.\n",
            "   * @param commitMessage Required commit message.\n",
            "   * @param kind The matching resource kind. Choices: \"effect\", \"fill\".\n",
            "   */\n",
            "  function update_shader(id: string, files?: Record<string, unknown>[], metadata?: Record<string, unknown>, commitMessage: string, kind: \"effect\" | \"fill\");\n"
        )
    );
}

#[test]
fn comments_wrap_without_truncation_and_parameter_continuations_align() {
    let description = "A detailed explanation that spans several lines without losing important words or instructions.";
    let tool = json!({"name":"read","description":description,"inputSchema":{"properties":{
        "nodeId":{"type":"string","description":description}
    }}});
    let style = Style {
        color: false,
        width: 60,
    };
    let output = style.render(&tool, true);
    for line in output
        .text
        .lines()
        .filter(|line| line.trim_start().starts_with('*'))
    {
        assert!(line.chars().count() <= 60, "{line}");
    }
    assert!(output.text.contains("   *               "));
    let words: Vec<_> = output.text.split_whitespace().collect();
    for word in description.split_whitespace() {
        assert!(words.contains(&word), "{word}");
    }
}

#[test]
fn multiline_and_long_parameter_names_preserve_every_word() {
    let property = "parameter_name_that_is_longer_than_the_documentation_column_width";
    let tool = json!({"name":"read","inputSchema":{"properties":{
        property:{"type":"string","description":"First paragraph.\nSecond paragraph with https://example.com/an/unbroken/technical/token."}
    }}});
    let output = Style {
        color: false,
        width: 40,
    }
    .render(&tool, true)
    .text;
    assert_eq!(output.matches(property).count(), 2);
    assert!(output.contains("First paragraph."));
    assert!(output.contains("Second paragraph"));
    assert!(output.contains("https://example.com/an/unbroken/technical/token."));
    assert!(!output.contains("* Second"));
}

#[test]
fn optional_field_limits_never_hide_required_parameters() {
    let tool = json!({"name":"configure","inputSchema":{"required":["last"],"properties":{
        "first":{"type":"string"},"second":{"type":"string"},"third":{"type":"string"},
        "fourth":{"type":"string"},"fifth":{"type":"string"},"sixth":{"type":"string"},
        "last":{"type":"boolean"}
    }}});
    let concise = plain().render(&tool, false);
    assert!(concise.hidden_parameters);
    assert!(concise.text.contains("last: boolean"));
    assert!(!concise.text.contains("sixth?"));
    let complete = plain().render(&tool, true);
    assert!(!complete.hidden_parameters);
    assert!(complete.text.contains("sixth?: string"));
    assert!(plain().brief(&tool).contains("configure(last: boolean)"));
}

#[test]
fn defaults_and_empty_tools_remain_readable() {
    let tool = json!({"name":"configure","inputSchema":{"properties":{
        "value":{"type":"string","default":null},"count":{"type":"integer","default":0},
        "label":{"type":"string","default":""}
    }}});
    let output = plain().render(&tool, false);
    assert!(output.text.contains("@param value? Default: null."));
    assert!(output.text.contains("@param count? Default: 0."));
    assert!(output.text.contains("@param label? Default: \"\"."));
    assert_eq!(
        plain().render(&json!({"name":"ping"}), false).text,
        "  function ping();\n"
    );
}

#[test]
fn unions_references_and_recursive_schemas_have_bounded_type_rendering() {
    let root = json!({"$defs":{
        "named":{"type":"string"},"cycle":{"$ref":"#/$defs/cycle"}
    }});
    assert_eq!(
        type_name(&json!({"$ref":"#/$defs/named"}), &root, 0),
        "string"
    );
    assert_eq!(
        type_name(&json!({"$ref":"#/$defs/cycle"}), &root, 0),
        "unknown"
    );
    assert_eq!(
        type_name(
            &json!({"type":"array","items":{"type":["string","null"]}}),
            &root,
            0
        ),
        "(string | null)[]"
    );
    assert_eq!(
        type_name(
            &json!({"anyOf":[{"type":"string"},{"type":"integer"},{"type":"string"}]}),
            &root,
            0
        ),
        "string | number"
    );
    assert_eq!(type_name(&json!({"type":[]}), &root, 0), "unknown");
    assert_eq!(type_name(&json!({"type":[null, 1]}), &root, 0), "unknown");
}

#[test]
fn terminal_styles_add_hierarchy_without_changing_plain_text() {
    let tool = json!({"name":"read","description":"Read a node.","inputSchema":{"required":["id"],"properties":{
        "id":{"type":"string","description":"The node ID."}
    }}});
    let colored = Style {
        color: true,
        width: 100,
    }
    .render(&tool, false)
    .text;
    assert!(colored.contains("\x1b[33m@param\x1b[0m"));
    assert!(colored.contains("\x1b[36mread\x1b[0m"));
    assert!(colored.contains("\x1b[90mRead a node.\x1b[0m"));
    assert!(!plain().render(&tool, false).text.contains('\x1b'));
}

#[test]
fn examples_are_valid_json_arguments_and_do_not_break_shell_quoting() {
    let tool = json!({"name":"write","inputSchema":{"required":["id","enabled","count","data"],"properties":{
        "id":{"type":"string","default":"owner's-id"},
        "enabled":{"type":"boolean"},"count":{"type":"integer"},"data":{"type":"object"}
    }}});
    let example = plain().example("fixture", &tool);
    let prefix = "mcp-pool call fixture.write --args '";
    let payload = example
        .strip_prefix(prefix)
        .and_then(|payload| payload.strip_suffix('\''))
        .and_then(|payload| serde_json::from_str::<Value>(payload).ok());
    assert_eq!(
        payload,
        Some(json!({"id":"owner's-id","enabled":true,"count":1,"data":{"key":"value"}}))
    );
}
