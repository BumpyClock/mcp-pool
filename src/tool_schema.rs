use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::{Call, coerce, unquote};

pub fn hydrate(call: &mut Call, schema: &Value) -> Result<()> {
    let properties = schema.get("properties").and_then(Value::as_object);
    let required = schema.get("required").and_then(Value::as_array);
    for flag in &call.generic_flags {
        if properties.is_none_or(|properties| {
            !properties
                .keys()
                .any(|name| name == flag || crate::tool_output::kebab(name) == *flag)
        }) {
            bail!("Unknown tool option '--{flag}'");
        }
    }
    if let Some(properties) = properties {
        for name in properties.keys() {
            let alias = crate::tool_output::kebab(name);
            if alias == *name || properties.contains_key(&alias) {
                continue;
            }
            if call.arguments.contains_key(name) && call.arguments.contains_key(&alias) {
                let latest = call
                    .argument_order
                    .iter()
                    .rev()
                    .find(|key| **key == alias || *key == name);
                if latest != Some(&alias) {
                    call.arguments.remove(&alias);
                    call.raw_values.remove(&alias);
                    call.literal_values.remove(&alias);
                    continue;
                }
                call.raw_values.remove(name);
                call.literal_values.remove(name);
            }
            if let Some(value) = call.arguments.remove(&alias) {
                call.arguments.insert(name.clone(), value);
            }
            if let Some(value) = call.raw_values.remove(&alias) {
                call.raw_values.insert(name.clone(), value);
            }
            if call.literal_values.remove(&alias) {
                call.literal_values.insert(name.clone());
            }
        }
    }
    let order: Vec<String> = properties
        .into_iter()
        .flat_map(|properties| properties.keys().cloned())
        .collect();
    for positional in std::mem::take(&mut call.positionals) {
        let name = order
            .iter()
            .find(|name| !call.arguments.contains_key(*name))
            .context("Too many positional arguments for this tool's schema")?;
        call.arguments.insert(name.clone(), positional);
    }
    if let Some(properties) = properties {
        for (name, property) in properties {
            let argument_name = if call.arguments.contains_key(name) {
                name.clone()
            } else {
                crate::tool_output::kebab(name)
            };
            if argument_name != *name {
                if let Some(value) = call.arguments.remove(&argument_name) {
                    call.arguments.insert(name.clone(), value);
                }
                if let Some(value) = call.raw_values.remove(&argument_name) {
                    call.raw_values.insert(name.clone(), value);
                }
                if call.literal_values.remove(&argument_name) {
                    call.literal_values.insert(name.clone());
                }
            }
            let Some(argument) = call.arguments.get_mut(name) else {
                continue;
            };
            if !call.raw_strings && property.get("type").and_then(Value::as_str) == Some("string") {
                if let Some(original) = call.raw_values.get(name) {
                    *argument = Value::String(if call.literal_values.contains(name) {
                        original.clone()
                    } else {
                        unquote(original)
                    });
                } else if !argument.is_string() {
                    *argument = Value::String(argument.to_string());
                }
            }
            if !call.raw_strings
                && !call.literal_values.contains(name)
                && property.get("type").and_then(Value::as_str) == Some("array")
                && argument.is_string()
            {
                let original = argument.as_str().context("Invalid array argument")?;
                *argument = Value::Array(
                    original
                        .split(',')
                        .map(|part| coerce(part.trim(), false))
                        .collect(),
                );
            }
            if let Some(kind) = property.get("type").and_then(Value::as_str) {
                let valid = match kind {
                    "string" => argument.is_string(),
                    "integer" => argument.is_i64() || argument.is_u64(),
                    "number" => argument.is_number(),
                    "boolean" => argument.is_boolean(),
                    "object" => argument.is_object(),
                    "array" => argument.is_array(),
                    "null" => argument.is_null(),
                    _ => true,
                };
                if !valid && !call.raw_strings {
                    bail!("Argument '{name}' must be {kind}");
                }
            }
            if let Some(choices) = property.get("enum").and_then(Value::as_array)
                && !choices.contains(argument)
            {
                bail!("Argument '{name}' is not an allowed enum value");
            }
        }
    }
    for name in required.into_iter().flatten().filter_map(Value::as_str) {
        if !call.arguments.contains_key(name) {
            bail!("Missing required argument '{name}'");
        }
    }
    if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
        for name in call.arguments.keys() {
            if properties.is_none_or(|properties| !properties.contains_key(name)) {
                bail!("Unknown argument '{name}' for this tool");
            }
        }
    }
    Ok(())
}
