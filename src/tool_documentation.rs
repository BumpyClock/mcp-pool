use std::io::{self, IsTerminal};

use serde_json::Value;

pub(crate) struct Style {
    color: bool,
    width: usize,
}

pub(crate) struct Documentation {
    pub text: String,
    pub hidden_parameters: bool,
}

impl Style {
    pub(crate) fn terminal(no_color: bool) -> Self {
        Self {
            color: !no_color
                && io::stdout().is_terminal()
                && std::env::var_os("NO_COLOR").is_none()
                && std::env::var("FORCE_COLOR").ok().as_deref() != Some("0")
                && std::env::var("TERM").ok().as_deref() != Some("dumb"),
            width: std::env::var("COLUMNS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(100)
                .clamp(40, 100),
        }
    }

    fn paint(&self, code: u8, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_owned()
        }
    }

    pub(crate) fn muted(&self, text: &str) -> String {
        self.paint(90, text)
    }

    pub(crate) fn heading(&self, text: &str) -> String {
        self.paint(1, text)
    }

    pub(crate) fn render(&self, tool: &Value, all_parameters: bool) -> Documentation {
        let options = crate::tool_output::options(tool);
        let displayed = display_options(&options, all_parameters);
        let hidden_parameters = displayed.len() < options.len();
        let mut lines = Vec::new();
        if let Some(description) = tool.get("description").and_then(Value::as_str) {
            for line in description.lines() {
                lines.extend(wrap(line, self.width.saturating_sub(5), "", ""));
            }
        }
        if !lines.is_empty() && !displayed.is_empty() {
            lines.push(String::new());
        }
        let mut parameters = Vec::new();
        for option in &displayed {
            let Some(property) = option.get("property").and_then(Value::as_str) else {
                continue;
            };
            let optional = if required(option) { "" } else { "?" };
            let mut text = option
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if let Some(default) = option.get("defaultValue") {
                append_detail(&mut text, &format!("Default: {default}."));
            }
            if let Some(choices) = option.get("enumValues").and_then(Value::as_array) {
                append_detail(
                    &mut text,
                    &format!(
                        "Choices: {}.",
                        choices
                            .iter()
                            .map(Value::to_string)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            }
            if !text.is_empty() {
                let prefix = format!("@param {property}{optional} ");
                let width = self.width.saturating_sub(5);
                let prefix_width = prefix.chars().count();
                let continuation = " ".repeat(prefix_width.min(width / 2));
                let wrapped = wrap(&text, width, &prefix, &continuation);
                parameters.push((prefix, wrapped));
            }
        }
        let mut output = String::new();
        if !lines.is_empty() || !parameters.is_empty() {
            output.push_str(&format!("  {}\n", self.muted("/**")));
            for line in lines {
                output.push_str(&format!(
                    "   {}{}\n",
                    self.muted("*"),
                    if line.is_empty() {
                        String::new()
                    } else {
                        format!(" {}", self.muted(&line))
                    }
                ));
            }
            for (prefix, wrapped) in parameters {
                for (index, line) in wrapped.into_iter().enumerate() {
                    let text = if index == 0 {
                        let remainder = line.strip_prefix(&prefix).unwrap_or_default();
                        let property = prefix
                            .strip_prefix("@param ")
                            .unwrap_or_default()
                            .trim_end();
                        format!(
                            "{} {} {}",
                            self.paint(33, "@param"),
                            self.paint(36, property),
                            self.muted(remainder)
                        )
                    } else {
                        self.muted(&line)
                    };
                    output.push_str(&format!("   {} {text}\n", self.muted("*")));
                }
            }
            output.push_str(&format!("   {}\n", self.muted("*/")));
        }
        output.push_str(&format!("  {}\n", self.signature(tool, &displayed)));
        Documentation {
            text: output,
            hidden_parameters,
        }
    }

    pub(crate) fn brief(&self, tool: &Value) -> String {
        let options = crate::tool_output::options(tool);
        self.signature(
            tool,
            &options
                .iter()
                .filter(|option| required(option))
                .collect::<Vec<_>>(),
        )
    }

    fn signature(&self, tool: &Value, options: &[&Value]) -> String {
        let schema = tool.get("inputSchema").unwrap_or(&Value::Null);
        let properties = schema.get("properties").and_then(Value::as_object);
        let parameters = options
            .iter()
            .filter_map(|option| {
                let property = option.get("property").and_then(Value::as_str)?;
                let descriptor = properties
                    .and_then(|properties| properties.get(property))
                    .unwrap_or(&Value::Null);
                Some(format!(
                    "{property}{}: {}",
                    if required(option) { "" } else { "?" },
                    self.muted(&type_name(descriptor, schema, 0))
                ))
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} {}({parameters});",
            self.muted("function"),
            self.paint(
                36,
                tool.get("name").and_then(Value::as_str).unwrap_or("tool")
            )
        )
    }

    pub(crate) fn example(&self, server: &str, tool: &Value) -> String {
        let properties = tool
            .get("inputSchema")
            .and_then(|schema| schema.get("properties"))
            .and_then(Value::as_object);
        let arguments = crate::tool_output::options(tool)
            .iter()
            .filter(|option| required(option))
            .filter_map(|option| {
                let property = option.get("property").and_then(Value::as_str)?;
                let kind = option.get("type").and_then(Value::as_str);
                if let Some(descriptor) = properties.and_then(|properties| properties.get(property))
                {
                    if let Some(value) = descriptor
                        .get("enum")
                        .and_then(Value::as_array)
                        .and_then(|values| values.first())
                    {
                        return Some((property.to_owned(), value.clone()));
                    }
                    if kind == Some("array")
                        && let Some(value) = descriptor
                            .get("items")
                            .and_then(|items| items.get("enum"))
                            .and_then(Value::as_array)
                            .and_then(|values| values.first())
                    {
                        return Some((property.to_owned(), Value::Array(vec![value.clone()])));
                    }
                }
                let example = option
                    .get("exampleValue")
                    .and_then(Value::as_str)
                    .unwrap_or("value");
                let value = match kind {
                    Some("array") => serde_json::from_str::<Value>(example)
                        .ok()
                        .filter(Value::is_array)
                        .unwrap_or_else(|| {
                            Value::Array(
                                example
                                    .split(',')
                                    .map(|item| {
                                        if option.get("arrayItemType").and_then(Value::as_str)
                                            == Some("string")
                                        {
                                            Value::String(item.to_owned())
                                        } else {
                                            serde_json::from_str(item)
                                                .unwrap_or_else(|_| Value::String(item.to_owned()))
                                        }
                                    })
                                    .collect(),
                            )
                        }),
                    Some("number" | "boolean" | "object") => serde_json::from_str(example)
                        .unwrap_or_else(|_| Value::String(example.to_owned())),
                    _ => Value::String(example.to_owned()),
                };
                Some((property.to_owned(), value))
            })
            .collect::<serde_json::Map<_, _>>();
        format!(
            "mcp-pool call {}.{} --args '{}'",
            server,
            tool.get("name").and_then(Value::as_str).unwrap_or("tool"),
            Value::Object(arguments)
                .to_string()
                .replace('\'', "\\u0027")
        )
    }
}

fn required(option: &Value) -> bool {
    option.get("required").and_then(Value::as_bool) == Some(true)
}

fn display_options(options: &[Value], all: bool) -> Vec<&Value> {
    let required_count = options.iter().filter(|option| required(option)).count();
    let mut optional_budget = 5usize.saturating_sub(required_count);
    options
        .iter()
        .filter(|option| {
            if all || options.len() <= 5 || required(option) {
                true
            } else if optional_budget != 0 {
                optional_budget -= 1;
                true
            } else {
                false
            }
        })
        .collect()
}

fn append_detail(text: &mut String, detail: &str) {
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(detail);
}

fn wrap(text: &str, width: usize, prefix: &str, continuation: &str) -> Vec<String> {
    let mut lines = Vec::new();
    for source in text.lines() {
        let mut line = if lines.is_empty() {
            prefix.to_owned()
        } else {
            continuation.to_owned()
        };
        for word in source.split_whitespace() {
            if !line.trim().is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                lines.push(line);
                line = continuation.to_owned();
            }
            if !line.is_empty() && !line.ends_with(' ') {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn type_name(schema: &Value, root: &Value, depth: usize) -> String {
    if depth >= 8 {
        return "unknown".to_owned();
    }
    if let Some(reference) = schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix('#'))
        && let Some(resolved) = root.pointer(reference)
    {
        return type_name(resolved, root, depth + 1);
    }
    for key in ["enum", "oneOf", "anyOf"] {
        if let Some(values) = schema.get(key).and_then(Value::as_array) {
            let mut types = Vec::new();
            for value in values {
                let kind = if key == "enum" {
                    if !(value.is_string()
                        || value.is_number()
                        || value.is_boolean()
                        || value.is_null())
                    {
                        continue;
                    }
                    value.to_string()
                } else {
                    type_name(value, root, depth + 1)
                };
                if !types.contains(&kind) {
                    types.push(kind);
                }
            }
            if !types.is_empty() {
                return types.join(" | ");
            }
        }
    }
    if let Some(kinds) = schema.get("type").and_then(Value::as_array) {
        let types = kinds
            .iter()
            .filter_map(Value::as_str)
            .map(|kind| primitive_type(kind, schema, root, depth))
            .collect::<Vec<_>>();
        return if types.is_empty() {
            "unknown".to_owned()
        } else {
            types.join(" | ")
        };
    }
    let kind = schema.get("type").and_then(Value::as_str).unwrap_or(
        if schema.get("properties").is_some() {
            "object"
        } else {
            "unknown"
        },
    );
    primitive_type(kind, schema, root, depth)
}

fn primitive_type(kind: &str, schema: &Value, root: &Value, depth: usize) -> String {
    match kind {
        "integer" | "number" => "number".to_owned(),
        "string" | "boolean" | "null" => kind.to_owned(),
        "object" => "Record<string, unknown>".to_owned(),
        "array" => {
            let item = type_name(schema.get("items").unwrap_or(&Value::Null), root, depth + 1);
            if item.contains(" | ") {
                format!("({item})[]")
            } else {
                format!("{item}[]")
            }
        }
        _ => "unknown".to_owned(),
    }
}

#[cfg(test)]
#[path = "tool_documentation_tests.rs"]
mod tests;
