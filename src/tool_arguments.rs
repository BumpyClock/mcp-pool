use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};

#[path = "tool_expression.rs"]
mod expression;
use expression::split_expression;
#[path = "tool_schema.rs"]
mod schema;
pub use schema::hydrate;
#[path = "tool_inputs.rs"]
mod inputs;
pub use inputs::{Inputs, merge_stdin};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Output {
    #[default]
    Auto,
    Text,
    Markdown,
    Json,
    Raw,
}

impl Output {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "text" => Ok(Self::Text),
            "markdown" => Ok(Self::Markdown),
            "json" => Ok(Self::Json),
            "raw" => Ok(Self::Raw),
            _ => bail!("--output must be auto, text, markdown, json, or raw"),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct AdHoc {
    pub url: Option<String>,
    pub command: Option<String>,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub headers: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    pub name: Option<String>,
    pub allow_http: bool,
    pub persist: Option<PathBuf>,
    pub description: Option<String>,
    pub transport: Option<String>,
}

impl AdHoc {
    pub fn present(&self) -> bool {
        self.url.is_some() || self.command.is_some()
    }

    pub fn consume(
        &mut self,
        token: &str,
        tokens: &mut std::collections::VecDeque<String>,
    ) -> Result<bool> {
        match token {
            "--http-url" | "--sse" => {
                self.url = Some(value(tokens, token)?);
                self.transport = Some(if token == "--sse" { "sse" } else { "http" }.to_owned());
            }
            "--stdio" => self.command = Some(value(tokens, token)?),
            "--stdio-arg" => self.arguments.push(value(tokens, token)?),
            "--cwd" => self.cwd = Some(PathBuf::from(value(tokens, token)?)),
            "--name" => self.name = Some(value(tokens, token)?),
            "--allow-http" => self.allow_http = true,
            "--env" | "--header" => {
                let entry = value(tokens, token)?;
                let (key, content) = entry
                    .split_once('=')
                    .or_else(|| entry.split_once(':'))
                    .context("--env/--header requires KEY=value")?;
                if key.trim().is_empty() {
                    bail!("Empty environment/header name");
                }
                let target = if token == "--env" {
                    &mut self.environment
                } else {
                    &mut self.headers
                };
                target.insert(key.trim().to_owned(), content.trim().to_owned());
            }
            "--persist" => self.persist = Some(PathBuf::from(value(tokens, token)?)),
            "--description" => self.description = Some(value(tokens, token)?),
            _ => return Ok(false),
        }
        Ok(true)
    }

    pub fn validate(&self) -> Result<()> {
        if !self.present()
            && (self.persist.is_some()
                || self.description.is_some()
                || self.name.is_some()
                || self.cwd.is_some()
                || self.allow_http
                || !self.arguments.is_empty()
                || !self.environment.is_empty()
                || !self.headers.is_empty())
        {
            bail!("Ad-hoc options require --stdio, --http-url, --sse, or an HTTP URL selector");
        }
        if self.command.is_some() && (!self.headers.is_empty() || self.allow_http) {
            bail!("--header and --allow-http require an ad-hoc HTTP/SSE target");
        }
        if self.url.is_some() && (!self.arguments.is_empty() || self.cwd.is_some()) {
            bail!("--stdio-arg and --cwd require an ad-hoc stdio command");
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct Call {
    pub server: Option<String>,
    pub tool: Option<String>,
    pub arguments: Map<String, Value>,
    pub argument_order: Vec<String>,
    pub positionals: Vec<Value>,
    pub raw_values: BTreeMap<String, String>,
    pub generic_flags: BTreeSet<String>,
    pub literal_values: BTreeSet<String>,
    pub stdin: bool,
    pub inputs: Inputs,
    pub output: Output,
    pub timeout: Option<u64>,
    pub no_oauth: bool,
    pub raw_strings: bool,
    pub no_coerce: bool,
    pub ephemeral: AdHoc,
    pub save_images: Option<PathBuf>,
    pub tail_log: bool,
}

pub fn value(tokens: &mut std::collections::VecDeque<String>, flag: &str) -> Result<String> {
    tokens
        .pop_front()
        .with_context(|| format!("Flag '{flag}' requires a value"))
}

pub fn positive_milliseconds(content: &str) -> Result<u64> {
    if content.is_empty()
        || content.starts_with('0')
        || !content.chars().all(|character| character.is_ascii_digit())
    {
        bail!("--timeout must be a positive integer (milliseconds)");
    }
    let result: u64 = content
        .parse()
        .context("--timeout must be a positive integer (milliseconds)")?;
    if result == 0 {
        bail!("--timeout must be a positive integer (milliseconds)");
    }
    if std::time::Instant::now()
        .checked_add(std::time::Duration::from_millis(result))
        .is_none()
    {
        bail!("Timeout exceeds the supported clock range");
    }
    Ok(result)
}

pub fn coerce(content: &str, raw: bool) -> Value {
    let content = content.trim();
    if matches!(content, "null" | "none") {
        return Value::Null;
    }
    if matches!(content, "true" | "false") {
        return Value::Bool(content == "true");
    }
    if content.starts_with('{') || content.starts_with('[') {
        return serde_json::from_str(content).unwrap_or_else(|_| Value::String(content.to_owned()));
    }
    if !raw
        && content.parse::<f64>().is_ok_and(|number| {
            if !number.is_finite() {
                return false;
            }
            let canonical = if number == 0.0 {
                "0".to_owned()
            } else if number.abs() < 1e-6 || number.abs() >= 1e21 {
                let scientific = format!("{number:e}");
                scientific
                    .split_once('e')
                    .map_or(scientific.clone(), |(mantissa, exponent)| {
                        format!(
                            "{mantissa}e{}{exponent}",
                            if exponent.starts_with('-') { "" } else { "+" }
                        )
                    })
            } else {
                number.to_string()
            };
            canonical == content
        })
        && let Ok(value) = serde_json::from_str(content)
    {
        return value;
    }
    Value::String(unquote(content))
}

fn unquote(content: &str) -> String {
    if content.starts_with('"')
        && let Ok(Value::String(text)) = serde_json::from_str(content)
    {
        return text;
    }
    if content.len() >= 2 && content.starts_with('\'') && content.ends_with('\'') {
        content
            .get(1..content.len().saturating_sub(1))
            .unwrap_or(content)
            .replace("\\'", "'")
    } else {
        content.to_owned()
    }
}

pub fn call(arguments: Vec<String>) -> Result<Call> {
    let mut result = Call::default();
    let mut tokens = std::collections::VecDeque::from(arguments);
    let mut positional = Vec::new();
    let mut literal = Vec::new();
    let mut named = Vec::new();
    while let Some(token) = tokens.pop_front() {
        if token == "--" {
            literal.extend(tokens);
            break;
        }
        if result.ephemeral.consume(&token, &mut tokens)? {
            continue;
        }
        match token.as_str() {
            "--server" | "--mcp" => result.server = Some(value(&mut tokens, &token)?),
            "--tool" => result.tool = Some(value(&mut tokens, &token)?),
            "--timeout" => {
                result.timeout = Some(positive_milliseconds(&value(&mut tokens, &token)?)?)
            }
            "--output" => result.output = Output::parse(&value(&mut tokens, &token)?)?,
            "--raw" => result.output = Output::Raw,
            "--raw-strings" => result.raw_strings = true,
            "--no-coerce" => {
                result.raw_strings = true;
                result.no_coerce = true;
            }
            "--no-oauth" => result.no_oauth = true,
            "--yes" => {}
            "--tail-log" => result.tail_log = true,
            "--save-images" => {
                result.save_images = Some(PathBuf::from(value(&mut tokens, &token)?))
            }
            "--args" | "--params" | "--json" => {
                let content = value(&mut tokens, &token)?;
                if content == "-" {
                    result.stdin = true;
                }
                result.inputs.json(&content)?;
            }
            _ if token.starts_with("--") => {
                let flag = token.trim_start_matches("--");
                if let Some((key, content)) = flag.split_once('=') {
                    result.generic_flags.insert(key.to_owned());
                    result.inputs.named(key.to_owned(), content.to_owned());
                } else {
                    result.generic_flags.insert(flag.to_owned());
                    let content = match tokens.front() {
                        Some(next) if !next.starts_with("--") => value(&mut tokens, &token)?,
                        _ => "true".to_owned(),
                    };
                    result.inputs.named(flag.to_owned(), content);
                }
            }
            _ => positional.push(token),
        }
    }
    let mut positional = std::collections::VecDeque::from(positional);
    if positional.front().is_some_and(|token| {
        token.find('(').is_some_and(|opening| {
            !token.starts_with("http") || token.find('?').is_none_or(|query| opening < query)
        })
    }) {
        let expression = value(&mut positional, "call expression")?;
        let opening = expression.find('(').context("Invalid call expression")?;
        if !expression.ends_with(')') {
            bail!("Unclosed call expression");
        }
        let selector = expression
            .get(..opening)
            .context("Invalid call expression")?;
        if !selector.contains('.') && (result.ephemeral.present() || result.server.is_some()) {
            if result.tool.as_deref().is_some_and(|tool| tool != selector) {
                bail!("Conflicting tool selector and flags");
            }
            result.tool = Some(selector.to_owned());
        } else {
            set_selector(&mut result, selector)?;
        }
        let contents = expression
            .get(opening + 1..expression.len().saturating_sub(1))
            .context("Invalid call expression")?;
        for part in split_expression(contents)? {
            if let Some((key, content)) = named_token(&part) {
                named.push((key, content));
            } else {
                result.positionals.push(call_value(part.trim(), &result));
            }
        }
    } else if positional.front().is_some_and(|token| {
        token.contains('.')
            && !token.contains('=')
            && (!token.contains(':') || token.starts_with("http"))
    }) && (result.server.is_none() || result.tool.is_none())
        && !result.ephemeral.present()
    {
        if let Some(selector) = positional.pop_front() {
            set_selector(&mut result, &selector)?;
        }
    } else if result.server.is_none()
        && !result.ephemeral.present()
        && let Some(selector) = positional.pop_front()
    {
        set_selector(&mut result, &selector)?;
    }
    if result.tool.is_none()
        && positional
            .front()
            .is_some_and(|token| named_token(token).is_none())
    {
        result.tool = positional.pop_front();
    }
    while let Some(token) = positional.pop_front() {
        if let Some((key, mut content)) = named_token(&token) {
            if content.is_empty() && token.ends_with(':') {
                content = value(&mut positional, &token)?;
            }
            named.push((key, content));
        } else {
            result.positionals.push(call_value(&token, &result));
        }
    }
    for (key, content) in named {
        result.inputs.named(key, content);
    }
    let mut inputs = std::mem::take(&mut result.inputs);
    inputs.prepare(&result)?;
    inputs.apply(&mut result, None)?;
    result.inputs = inputs;
    result.positionals.extend(literal.iter().map(|content| {
        if result.no_coerce {
            Value::String(content.trim().to_owned())
        } else {
            coerce(content, result.raw_strings)
        }
    }));
    if result.tool.as_deref() == Some("") {
        bail!("Tool name cannot be empty");
    }
    if result.server.is_none() && !result.ephemeral.present() {
        bail!("Missing server. Use server.tool or --server NAME");
    }
    result.ephemeral.validate()?;
    Ok(result)
}

fn call_value(content: &str, call: &Call) -> Value {
    if call.no_coerce {
        Value::String(content.trim().to_owned())
    } else {
        coerce(content, call.raw_strings)
    }
}

fn set_selector(result: &mut Call, selector: &str) -> Result<()> {
    if let Some((url, tool)) = crate::mcp_cli::selector::split_http(selector)? {
        result.ephemeral.url = Some(url);
        if let Some(tool) = tool {
            result.tool = Some(tool);
        }
        return Ok(());
    }
    if selector.chars().any(char::is_whitespace)
        || selector.starts_with(['/', '~'])
        || selector.starts_with(".\\")
        || selector.starts_with("./")
        || selector.starts_with("..\\")
        || selector.starts_with("../")
        || selector.starts_with("\\\\")
        || selector.get(1..3) == Some(":\\")
    {
        result.ephemeral.command = Some(selector.to_owned());
        return Ok(());
    }
    if let Some((server, tool)) = selector.rsplit_once('.') {
        if result
            .server
            .as_deref()
            .is_some_and(|existing| existing != server)
            || result
                .tool
                .as_deref()
                .is_some_and(|existing| existing != tool)
        {
            bail!("Conflicting server/tool selector and flags");
        }
        result.server = Some(server.to_owned());
        result.tool = Some(tool.to_owned());
    } else if result.server.is_some() {
        if result
            .tool
            .as_deref()
            .is_some_and(|existing| existing != selector)
        {
            bail!("Conflicting tool selector and flags");
        }
        result.tool = Some(selector.to_owned());
    } else {
        result.server = Some(selector.to_owned());
    }
    Ok(())
}

fn named_token(token: &str) -> Option<(String, String)> {
    let (key, content) = token.split_once('=').or_else(|| token.split_once(':'))?;
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|character| character.is_alphanumeric() || matches!(character, '_' | '-' | '.'))
    {
        return None;
    }

    Some((key.to_owned(), content.trim().to_owned()))
}

fn named_value(content: &str) -> Result<Option<String>> {
    if let Some(literal) = content.strip_prefix("@@") {
        return Ok(Some(format!("@{literal}")));
    }
    let Some(path) = content.strip_prefix('@') else {
        return Ok(None);
    };
    if path.is_empty() {
        bail!("Named @file arguments require a file path; use @@ for a literal @");
    }
    let metadata = std::fs::metadata(path).context("Inspecting named argument file")?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
        bail!("Named argument input must be a regular UTF-8 file no larger than 16 MiB");
    }
    Ok(Some(
        std::fs::read_to_string(path).context("Reading named argument UTF-8 file")?,
    ))
}

pub fn merge_json(target: &mut Map<String, Value>, content: &str) -> Result<()> {
    let parsed: Value = serde_json::from_str(content).context("Arguments must be valid JSON")?;
    let object = parsed
        .as_object()
        .context("Arguments must be a JSON object")?;
    target.extend(object.clone());
    Ok(())
}

#[cfg(test)]
#[path = "tool_argument_tests.rs"]
mod tests;
