use anyhow::Result;
use serde_json::{Map, Value};

use super::schema_view::{Kind, SchemaView, kinds};
use super::{Call, call_value, coerce, merge_json, named_value, unquote};

#[derive(Debug)]
enum Input {
    Object(Map<String, Value>),
    Stdin(Map<String, Value>),
    Named {
        key: String,
        raw: String,
    },
    Resolved {
        key: String,
        value: Value,
        provenance: Provenance,
    },
}

#[derive(Debug)]
pub(super) enum Provenance {
    Json,
    Named(String),
    Literal,
}

#[derive(Debug, Default)]
pub struct Inputs(Vec<(usize, Input)>);

impl Inputs {
    pub(crate) fn has_stdin(&self) -> bool {
        self.0
            .iter()
            .any(|(_, input)| matches!(input, Input::Stdin(_)))
    }

    pub(crate) fn has_explicit_json(&self) -> bool {
        self.0
            .iter()
            .any(|(_, input)| matches!(input, Input::Object(_) | Input::Stdin(_)))
    }

    pub(super) fn json(&mut self, order: usize, contents: &str) -> Result<()> {
        let input = if contents == "-" {
            Input::Stdin(Map::new())
        } else {
            let mut object = Map::new();
            merge_json(&mut object, contents)?;
            Input::Object(object)
        };
        self.0.push((order, input));
        Ok(())
    }

    pub(super) fn named(&mut self, order: usize, key: String, raw: String) {
        self.0.push((order, Input::Named { key, raw }));
    }

    pub(super) fn prepare(&mut self, call: &Call) -> Result<()> {
        self.0.sort_by_key(|(order, _)| *order);
        for (_, input) in &mut self.0 {
            if let Input::Named { key, raw } = input {
                let (value, provenance) = match named_value(raw)? {
                    Some(contents) => (Value::String(contents), Provenance::Literal),
                    None => (call_value(raw, call), Provenance::Named(raw.clone())),
                };
                *input = Input::Resolved {
                    key: key.clone(),
                    value,
                    provenance,
                };
            }
        }
        Ok(())
    }

    pub(super) fn apply(&self, call: &mut Call, schema: Option<&SchemaView<'_>>) -> Result<()> {
        call.arguments.clear();
        for (_, input) in &self.0 {
            match input {
                Input::Object(values) | Input::Stdin(values) => {
                    for (key, value) in values {
                        Self::insert(call, schema, key, value, &Provenance::Json)?;
                    }
                }
                Input::Resolved {
                    key,
                    value,
                    provenance,
                } => {
                    Self::insert(call, schema, key, value, provenance)?;
                }
                Input::Named { .. } => {
                    anyhow::bail!("Tool inputs were not prepared before application")
                }
            }
        }
        Ok(())
    }

    fn insert(
        call: &mut Call,
        schema: Option<&SchemaView<'_>>,
        key: &str,
        value: &Value,
        source: &Provenance,
    ) -> Result<()> {
        let name = match schema {
            Some(schema) => schema.property_name(key)?,
            None => key,
        };
        let descriptor = schema.and_then(|schema| {
            schema
                .property(name)
                .map(|property| schema.resolve(property))
        });
        let mut value = value.clone();
        if let Some(descriptor) = descriptor {
            hydrate_value(&mut value, descriptor, source, call.raw_strings);
        }
        call.arguments.insert(name.to_owned(), value);
        Ok(())
    }
}

pub(super) fn hydrate_value(
    value: &mut Value,
    descriptor: &Value,
    source: &Provenance,
    raw_strings: bool,
) {
    if raw_strings {
        return;
    }
    let kinds = kinds(descriptor);
    let kind = kinds.iter().find(|kind| **kind != Kind::Null);
    if kind == Some(&Kind::String)
        && (kinds.len() == 1 || !kinds.iter().any(|kind| kind.accepts(value)))
    {
        if let Provenance::Named(raw) = source {
            *value = Value::String(unquote(raw));
        } else if !value.is_string() {
            *value = Value::String(value.to_string());
        }
    }
    if kind == Some(&Kind::Array)
        && (kinds.len() == 1 || !kinds.iter().any(|kind| kind.accepts(value)))
        && !matches!(source, Provenance::Literal)
        && let Some(original) = value.as_str()
    {
        *value = Value::Array(
            original
                .split(',')
                .map(|part| coerce(part.trim(), false))
                .collect(),
        );
    }
}

pub fn merge_stdin(call: &mut Call, contents: &str) -> Result<()> {
    let mut values = Map::new();
    merge_json(&mut values, contents)?;
    let mut inputs = std::mem::take(&mut call.inputs);
    for (_, input) in &mut inputs.0 {
        if let Input::Stdin(previous) = input {
            *previous = values.clone();
        }
    }
    let outcome = inputs.apply(call, None);
    call.inputs = inputs;
    outcome
}
