use anyhow::{Context, Result, bail};
use serde_json::Value;

use super::Call;
use super::inputs::{Provenance, hydrate_value};
use super::schema_view::{SchemaView, kinds};

pub fn hydrate(call: &mut Call, schema: &Value) -> Result<()> {
    let view = SchemaView::new(schema);
    for flag in &call.generic_flags {
        if view.property(view.property_name(flag)?).is_none() {
            bail!("Unknown tool option '--{flag}'");
        }
    }
    let inputs = std::mem::take(&mut call.inputs);
    let outcome = inputs.apply(call, Some(&view));
    call.inputs = inputs;
    outcome?;
    for mut positional in std::mem::take(&mut call.positionals) {
        let (name, descriptor) = view
            .properties()
            .find(|(name, _)| !call.arguments.contains_key(*name))
            .context("Too many positional arguments for this tool's schema")?;
        hydrate_value(
            &mut positional,
            view.resolve(descriptor),
            &Provenance::Json,
            call.raw_strings,
        );
        call.arguments.insert(name.clone(), positional);
    }
    for (name, property) in view.properties() {
        let Some(argument) = call.arguments.get(name) else {
            continue;
        };
        let descriptor = view.resolve(property);
        let kinds = kinds(descriptor);
        if !call.raw_strings
            && !kinds.is_empty()
            && !kinds.iter().any(|kind| kind.accepts(argument))
        {
            let kind = descriptor.get("type").context("Schema type missing")?;
            let kind = kind
                .as_str()
                .map_or_else(|| kind.to_string(), str::to_owned);
            bail!("Argument '{name}' must be {kind}");
        }
        if let Some(choices) = descriptor.get("enum").and_then(Value::as_array)
            && !choices.contains(argument)
        {
            bail!("Argument '{name}' is not an allowed enum value");
        }
    }
    for name in schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if !call.arguments.contains_key(name) {
            bail!("Missing required argument '{name}'");
        }
    }
    if schema.get("additionalProperties") == Some(&Value::Bool(false)) {
        for name in call.arguments.keys() {
            if view.property(name).is_none() {
                bail!("Unknown argument '{name}' for this tool");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "tool_schema_tests.rs"]
mod tests;
