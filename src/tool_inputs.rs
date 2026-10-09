use anyhow::Result;
use serde_json::{Map, Value};

use super::{Call, call_value, merge_json, named_value};

#[derive(Debug)]
enum Input {
    Object(Map<String, Value>),
    Stdin,
    Named(String, String),
    Resolved {
        key: String,
        value: Value,
        raw: String,
        literal: bool,
    },
}

#[derive(Debug, Default)]
pub struct Inputs(Vec<Input>);

impl Inputs {
    pub(super) fn json(&mut self, contents: &str) -> Result<()> {
        if contents == "-" {
            self.0.push(Input::Stdin);
        } else {
            let mut object = Map::new();
            merge_json(&mut object, contents)?;
            self.0.push(Input::Object(object));
        }
        Ok(())
    }

    pub(super) fn named(&mut self, key: String, value: String) {
        self.0.push(Input::Named(key, value));
    }

    pub(super) fn prepare(&mut self, call: &Call) -> Result<()> {
        for input in &mut self.0 {
            if let Input::Named(key, raw) = input {
                let expanded = named_value(raw)?;
                let literal = expanded.is_some();
                let raw = expanded.unwrap_or_else(|| raw.clone());
                let value = if literal {
                    Value::String(raw.clone())
                } else {
                    call_value(&raw, call)
                };
                *input = Input::Resolved {
                    key: key.clone(),
                    value,
                    raw,
                    literal,
                };
            }
        }
        Ok(())
    }

    pub(super) fn apply(&self, call: &mut Call, stdin: Option<&str>) -> Result<()> {
        let mut from_stdin = Map::new();
        if let Some(contents) = stdin {
            merge_json(&mut from_stdin, contents)?;
        }
        call.arguments.clear();
        call.argument_order.clear();
        call.raw_values.clear();
        call.literal_values.clear();
        for input in &self.0 {
            match input {
                Input::Object(values) => Self::merge(call, values),
                Input::Stdin => Self::merge(call, &from_stdin),
                Input::Resolved {
                    key,
                    value,
                    raw,
                    literal,
                } => {
                    call.argument_order.push(key.clone());
                    call.arguments.insert(key.clone(), value.clone());
                    call.raw_values.insert(key.clone(), raw.clone());
                    call.literal_values.remove(key);
                    if *literal {
                        call.literal_values.insert(key.clone());
                    }
                }
                Input::Named(..) => {
                    anyhow::bail!("Tool inputs were not prepared before application")
                }
            }
        }
        Ok(())
    }

    fn merge(call: &mut Call, values: &Map<String, Value>) {
        for (key, value) in values {
            call.argument_order.push(key.clone());
            call.raw_values.remove(key);
            call.literal_values.remove(key);
            call.arguments.insert(key.clone(), value.clone());
        }
    }
}

pub fn merge_stdin(call: &mut Call, contents: &str) -> Result<()> {
    let inputs = std::mem::take(&mut call.inputs);
    let outcome = inputs.apply(call, Some(contents));
    call.inputs = inputs;
    outcome
}
