use std::collections::BTreeMap;

use anyhow::{Result, bail};
use serde_json::{Map, Value};

pub(crate) struct SchemaView<'a> {
    root: &'a Value,
    properties: Option<&'a Map<String, Value>>,
    aliases: BTreeMap<String, Vec<&'a str>>,
}

impl<'a> SchemaView<'a> {
    pub(crate) fn new(root: &'a Value) -> Self {
        let properties = root.get("properties").and_then(Value::as_object);
        let mut aliases: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        for name in properties.into_iter().flat_map(Map::keys) {
            aliases.entry(kebab(name)).or_default().push(name);
        }
        Self {
            root,
            properties,
            aliases,
        }
    }

    pub(crate) fn properties(&self) -> impl Iterator<Item = (&'a String, &'a Value)> {
        self.properties.into_iter().flat_map(Map::iter)
    }

    pub(crate) fn required(&self, name: &str) -> bool {
        self.root
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|values| values.iter().any(|value| value.as_str() == Some(name)))
    }

    pub(crate) fn property(&self, name: &str) -> Option<&'a Value> {
        self.properties?.get(name)
    }

    pub(crate) fn resolve(&self, descriptor: &'a Value) -> &'a Value {
        resolve_schema(descriptor, self.root, 0).map_or(&Value::Null, |(schema, _)| schema)
    }

    pub(crate) fn property_name<'b>(&'b self, name: &'b str) -> Result<&'b str> {
        if self.property(name).is_some() {
            return Ok(name);
        }
        match self.aliases.get(name).map(Vec::as_slice) {
            Some([property]) => Ok(property),
            Some(properties) => bail!(
                "Tool option '{name}' is ambiguous; matches: {}",
                properties.join(", ")
            ),
            None => Ok(name),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    String,
    Integer,
    Number,
    Boolean,
    Object,
    Array,
    Null,
}

impl Kind {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "string" => Some(Self::String),
            "integer" => Some(Self::Integer),
            "number" => Some(Self::Number),
            "boolean" => Some(Self::Boolean),
            "object" => Some(Self::Object),
            "array" => Some(Self::Array),
            "null" => Some(Self::Null),
            _ => None,
        }
    }

    pub(crate) fn accepts(self, value: &Value) -> bool {
        match self {
            Self::String => value.is_string(),
            Self::Integer => value.is_i64() || value.is_u64(),
            Self::Number => value.is_number(),
            Self::Boolean => value.is_boolean(),
            Self::Object => value.is_object(),
            Self::Array => value.is_array(),
            Self::Null => value.is_null(),
        }
    }
}

pub(crate) fn kinds(schema: &Value) -> Vec<Kind> {
    let Some(value) = schema.get("type") else {
        return Vec::new();
    };
    if let Some(name) = value.as_str() {
        return Kind::parse(name).into_iter().collect();
    }
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(Kind::parse)
        .collect()
}

pub(crate) fn type_name(schema: &Value, root: &Value, depth: usize) -> String {
    let Some((schema, depth)) = resolve_schema(schema, root, depth) else {
        return "unknown".to_owned();
    };
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

pub(crate) fn resolve_schema<'a>(
    mut schema: &'a Value,
    root: &'a Value,
    mut depth: usize,
) -> Option<(&'a Value, usize)> {
    while depth < 8 {
        let Some(reference) = schema.get("$ref").and_then(Value::as_str) else {
            return Some((schema, depth));
        };
        let Some(resolved) = reference
            .strip_prefix('#')
            .and_then(|reference| root.pointer(reference))
        else {
            return Some((schema, depth));
        };
        schema = resolved;
        depth += 1;
    }
    None
}

pub(crate) fn example_descriptor(schema: &Value, root: &Value, depth: usize) -> Value {
    let Some((schema, depth)) = resolve_schema(schema, root, depth) else {
        return Value::Null;
    };
    let mut descriptor = schema.clone();
    if let Some(items) = schema.get("items")
        && let Some(object) = descriptor.as_object_mut()
    {
        object.insert(
            "items".to_owned(),
            example_descriptor(items, root, depth + 1),
        );
    }
    descriptor
}

fn primitive_type(kind: &str, schema: &Value, root: &Value, depth: usize) -> String {
    match Kind::parse(kind) {
        Some(Kind::Integer | Kind::Number) => "number".to_owned(),
        Some(Kind::String | Kind::Boolean | Kind::Null) => kind.to_owned(),
        Some(Kind::Object) => "Record<string, unknown>".to_owned(),
        Some(Kind::Array) => {
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

pub(crate) fn projection_type(value: Option<&Value>) -> &'static str {
    let kind = value.and_then(|value| {
        value
            .as_str()
            .and_then(Kind::parse)
            .filter(|kind| *kind != Kind::Null)
            .or_else(|| {
                value.as_array().and_then(|values| {
                    values
                        .iter()
                        .filter_map(Value::as_str)
                        .filter_map(Kind::parse)
                        .find(|kind| *kind != Kind::Null)
                })
            })
    });
    match kind {
        Some(Kind::Integer | Kind::Number) => "number",
        Some(Kind::String) => "string",
        Some(Kind::Boolean) => "boolean",
        Some(Kind::Array) => "array",
        Some(Kind::Object) => "object",
        _ => "unknown",
    }
}

pub(crate) fn kebab(property: &str) -> String {
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
