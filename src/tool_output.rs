use anyhow::Result;
use serde_json::{Value, json};

use crate::tool_arguments::Output;

#[derive(Default)]
struct Content {
    content_found: bool,
    structured: Option<Value>,
    json: Vec<Value>,
    text: Vec<String>,
    markdown: Vec<String>,
}

pub fn failed(result: &Value) -> bool {
    result.get("isError").and_then(Value::as_bool) == Some(true)
        || (result.get("jsonrpc").and_then(Value::as_str) == Some("2.0")
            && result.get("error").is_some_and(|error| !error.is_null()))
}

pub fn render(result: &Value, output: Output) -> Result<String> {
    if output == Output::Raw {
        return Ok(serde_json::to_string_pretty(result)?);
    }
    let mut content = Content::default();
    collect_envelope(result, &mut content, 0);
    if let Some(resources) = result.get("contents").and_then(Value::as_array) {
        for resource in resources {
            collect_resource(resource, &mut content);
        }
    }
    let mut json = content
        .structured
        .as_ref()
        .and_then(structured_json)
        .or_else(|| {
            if content.json.len() == 1 {
                content.json.first().cloned()
            } else if content.json.is_empty() {
                None
            } else {
                Some(Value::Array(content.json.clone()))
            }
        })
        .or_else(|| {
            result
                .as_str()
                .and_then(|text| serde_json::from_str(text).ok())
        });
    let structured_markdown = content
        .structured
        .as_ref()
        .and_then(|value| value.get("markdown"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let markdown = structured_markdown
        .or_else(|| (!content.markdown.is_empty()).then(|| content.markdown.join("\n")));
    let text = if result.is_string() {
        result.as_str().map(str::to_owned)
    } else if !content.text.is_empty() {
        Some(content.text.join("\n"))
    } else {
        content
            .structured
            .as_ref()
            .and_then(|value| {
                value
                    .as_str()
                    .or_else(|| value.get("text").and_then(Value::as_str))
            })
            .map(str::to_owned)
    };
    json = json
        .or_else(|| {
            text.as_deref()
                .and_then(|text| serde_json::from_str(text).ok())
        })
        .or_else(|| {
            markdown
                .as_deref()
                .and_then(|text| serde_json::from_str(text).ok())
        });
    let extracted_text = match output {
        Output::Text => text.or(markdown),
        Output::Markdown => markdown.or(text),
        Output::Auto if json.is_none() => markdown.or(text),
        _ => None,
    };
    if let Some(text) = extracted_text {
        return Ok(text);
    }
    Ok(serde_json::to_string_pretty(
        json.as_ref().unwrap_or(result),
    )?)
}

fn structured_json(value: &Value) -> Option<Value> {
    if value.is_null() {
        return None;
    }
    if let Some(text) = value.as_str() {
        return serde_json::from_str(text).ok();
    }
    if !value.is_object() && !value.is_array() {
        return None;
    }
    Some(unwrap_json(value))
}

fn unwrap_json(value: &Value) -> Value {
    if let Some(json) = value.get("json") {
        return json.clone();
    }
    if let Some(object) = value.as_object()
        && object.len() == 1
        && let Some(data) = object.get("data")
    {
        return data.clone();
    }
    value.clone()
}

fn collect_envelope(result: &Value, content: &mut Content, depth: usize) {
    if content.structured.is_none() {
        content.structured = result
            .get("structuredContent")
            .filter(|value| !value.is_null())
            .cloned();
    }
    if let Some(entries) = result
        .get("content")
        .and_then(Value::as_array)
        .filter(|_| !content.content_found)
    {
        content.content_found = true;
        for entry in entries {
            match entry.get("type").and_then(Value::as_str) {
                Some("resource") => {
                    if let Some(resource) = entry.get("resource") {
                        collect_resource(resource, content);
                    }
                }
                Some("text" | "markdown") => {
                    if let Some(text) = entry.get("text").and_then(Value::as_str) {
                        content.text.push(text.to_owned());
                        if entry.get("type").and_then(Value::as_str) == Some("markdown") {
                            content.markdown.push(text.to_owned());
                        }
                        if let Ok(parsed) = serde_json::from_str(text) {
                            content.json.push(parsed);
                        }
                    }
                }
                Some("json") => {
                    let parsed = unwrap_json(entry);
                    if entry.get("json").is_some() || entry.get("data").is_some() {
                        content.json.push(parsed);
                    }
                }
                _ => {
                    if let Some(text) = entry.as_str()
                        && let Ok(parsed) = serde_json::from_str(text)
                    {
                        content.json.push(parsed);
                    }
                }
            }
        }
    }
    if depth < 2 {
        for key in ["raw", "result"] {
            if let Some(nested) = result.get(key) {
                collect_envelope(nested, content, depth + 1);
            }
        }
    }
}

fn collect_resource(resource: &Value, content: &mut Content) {
    if let Some(text) = resource.get("text").and_then(Value::as_str) {
        content.text.push(text.to_owned());
        if resource
            .get("mimeType")
            .and_then(Value::as_str)
            .is_some_and(|mime| mime.to_lowercase().contains("markdown"))
        {
            content.markdown.push(text.to_owned());
        }
        if let Ok(parsed) = serde_json::from_str(text) {
            content.json.push(parsed);
        }
    } else if resource.get("blob").and_then(Value::as_str).is_some() {
        let uri = resource.get("uri").and_then(Value::as_str).unwrap_or("");
        content.text.push(format!("[Binary resource: {uri}]"));
    }
}

pub fn options(tool: &Value) -> Vec<Value> {
    let Some(schema) = tool.get("inputSchema") else {
        return Vec::new();
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let required = schema.get("required").and_then(Value::as_array);
    properties.iter().map(|(property, descriptor)| {
        let kind = schema_type(descriptor.get("type"));
        let cli_name = kebab(property);
        let enum_values: Vec<&str> = descriptor.get("enum")
            .or_else(|| if kind == "array" { descriptor.get("items").and_then(|items| items.get("enum")) } else { None })
            .and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        let placeholder = if !enum_values.is_empty() {
            format!("<{cli_name}:{}{}>", enum_values.join("|"), if kind == "array" { ",..." } else { "" })
        } else { match kind {
            "number" => format!("<{cli_name}:number>"),
            "boolean" => format!("<{cli_name}:true|false>"),
            "array" => format!("<{cli_name}:value1,value2>"),
            "object" => format!("<{cli_name}:json>"),
            _ => descriptor.get("format").and_then(Value::as_str)
                .map_or_else(|| format!("<{cli_name}>"), |format| format!("<{cli_name}:{format}>")),
        }};
        let mut option = json!({
            "property":property, "cliName":cli_name, "required":required.is_some_and(|values| values.iter().any(|value| value.as_str() == Some(property))),
            "type":kind, "placeholder":placeholder,
        });
        if let Some(object) = option.as_object_mut() {
            for (source, target) in [("description","description"),("default","defaultValue")] {
                if let Some(value) = descriptor.get(source) { object.insert(target.to_owned(), value.clone()); }
            }
            if !enum_values.is_empty() {
                object.insert("enumValues".to_owned(), json!(enum_values));
            }
            if let Some(format) = descriptor.get("format").and_then(Value::as_str) {
                let display = match format {
                    "date-time" | "iso-8601" => "ISO 8601".to_owned(),
                    "uuid" => "UUID".to_owned(),
                    _ => format.split(['_', '-']).map(|word| {
                        let mut characters = word.chars();
                        characters.next().map_or_else(String::new, |first| first.to_uppercase().collect::<String>() + characters.as_str())
                    }).collect::<Vec<_>>().join(" "),
                };
                object.insert("formatHint".to_owned(), json!(display));
            }
            let item_kind = schema_type(descriptor.get("items").and_then(|items| items.get("type")));
            if kind == "array" {
                object.insert("arrayItemType".to_owned(), json!(item_kind));
                let item_types = descriptor.get("items").and_then(|items| items.get("type"));
                let strict = item_types.is_some_and(|types| types.as_str() == Some("boolean")
                    || types.as_array().is_some_and(|types| !types.is_empty() && types.iter().all(|kind| kind.as_str() == Some("boolean"))));
                object.insert("strictBooleanArray".to_owned(), json!(strict));
            }
            let example = enum_values.first().map(|value| (*value).to_owned())
                .or_else(|| descriptor.get("default").map(|value| value.as_str().map_or_else(|| value.to_string(), str::to_owned)))
                .or_else(|| match kind {
                    "number" => Some("1".to_owned()), "boolean" => Some("true".to_owned()),
                    "object" => Some(r#"{"key":"value"}"#.to_owned()),
                    "array" => Some(match item_kind { "number" => "1,2", "boolean" => "true,false", "object" => r#"[{"key":"value"}]"#, _ => "value1,value2" }.to_owned()),
                    _ if property.to_ascii_lowercase().contains("path") => Some("/path/to/file.md".to_owned()),
                    _ if property.to_ascii_lowercase().contains("id") => Some("example-id".to_owned()),
                    _ => None,
                });
            if let Some(example) = example {
                object.insert("exampleValue".to_owned(), json!(example));
            }
        }
        option
    }).collect()
}

fn schema_type(value: Option<&Value>) -> &'static str {
    let value = value.and_then(|value| {
        value
            .as_array()
            .and_then(|values| {
                values.iter().find(|value| {
                    matches!(
                        value.as_str(),
                        Some("integer" | "number" | "string" | "boolean" | "array" | "object")
                    )
                })
            })
            .or(Some(value))
    });
    match value.and_then(Value::as_str) {
        Some("integer" | "number") => "number",
        Some("string") => "string",
        Some("boolean") => "boolean",
        Some("array") => "array",
        Some("object") => "object",
        _ => "unknown",
    }
}

pub fn kebab(property: &str) -> String {
    let mut result = String::new();
    let mut previous: Option<char> = None;
    let mut characters = property.chars().peekable();
    while let Some(character) = characters.next() {
        if character.is_uppercase() {
            let boundary = previous
                .is_some_and(|previous| previous.is_lowercase() || previous.is_ascii_digit())
                || (previous.is_some_and(char::is_uppercase)
                    && characters.peek().is_some_and(|next| next.is_lowercase()));
            if boundary && !result.is_empty() && !result.ends_with('-') {
                result.push('-');
            }
            result.extend(character.to_lowercase());
        } else if matches!(character, '_' | ' ' | '.' | '-') {
            if !result.is_empty() && !result.ends_with('-') {
                result.push('-');
            }
        } else {
            result.push(character);
        }
        previous = Some(character);
    }
    let result = result.trim_matches('-').to_owned();
    if result.is_empty() {
        "option".to_owned()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_contract_extracts_structured_and_text_content() -> Result<()> {
        let result = json!({"content":[{"type":"text","text":"human"}],"structuredContent":{"data":{"answer":42}}});
        assert_eq!(
            serde_json::from_str::<Value>(&render(&result, Output::Json)?)?,
            json!({"answer":42})
        );
        assert_eq!(render(&result, Output::Text)?, "human");
        assert_eq!(
            serde_json::from_str::<Value>(&render(&result, Output::Raw)?)?,
            result
        );
        let result = json!({"content":[{"type":"text","text":"{\"answer\":42}"}]});
        assert_eq!(
            serde_json::from_str::<Value>(&render(&result, Output::Auto)?)?,
            json!({"answer":42})
        );
        assert_eq!(
            render(
                &json!({"contents":[{"uri":"test://a","mimeType":"text/markdown","text":"# Header"}]}),
                Output::Markdown
            )?,
            "# Header"
        );
        assert_eq!(
            render(&json!("{\"answer\":42}"), Output::Json)?,
            "{\n  \"answer\": 42\n}"
        );
        assert_eq!(
            render(
                &json!({"raw":{"content":[{"type":"text","text":"nested"}]},"structuredContent":"plain"}),
                Output::Text
            )?,
            "nested"
        );
        assert_eq!(
            render(&json!({"structuredContent":"plain"}), Output::Auto)?,
            "plain"
        );
        let markdown = json!({"structuredContent":{"markdown":"# fallback"}});
        assert_eq!(render(&markdown, Output::Text)?, "# fallback");
        let text = json!({"content":[{"type":"text","text":"fallback"}]});
        assert_eq!(render(&text, Output::Markdown)?, "fallback");
        Ok(())
    }

    #[test]
    fn errors_are_not_success_shaped() -> Result<()> {
        let result = json!({"isError":true,"content":[{"type":"text","text":"broken"}]});
        assert!(failed(&result));
        assert_eq!(render(&result, Output::Text)?, "broken");
        assert_eq!(
            serde_json::from_str::<Value>(&render(&result, Output::Json)?)?,
            result
        );
        assert!(failed(
            &json!({"jsonrpc":"2.0","error":{"code":-32603,"message":"broken"}})
        ));
        assert!(!failed(&json!({"error":"a normal application payload"})));
        Ok(())
    }

    #[test]
    fn machine_options_match_reference_descriptor_shape() {
        let metadata = options(
            &json!({"name":"search","inputSchema":{"type":"object","required":["count"],
                "properties":{"count":{"type":"integer"},"tags":{"type":["array","null"],"items":{"type":"string","enum":["fast","slow"]}}}
            }}),
        );
        assert_eq!(
            metadata,
            vec![
                json!({"property":"count","cliName":"count","required":true,"type":"number","placeholder":"<count:number>","exampleValue":"1"}),
                json!({"property":"tags","cliName":"tags","required":false,"type":"array","placeholder":"<tags:fast|slow,...>",
                "enumValues":["fast","slow"],"arrayItemType":"string","strictBooleanArray":false,"exampleValue":"fast"}),
            ]
        );
        assert_eq!(kebab("Page__Size"), "page-size");
    }
}
