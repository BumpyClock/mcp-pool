use std::path::Path;

use super::ImportKind;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};

pub(super) fn entry(
    kind: ImportKind,
    value: &Value,
    name: &str,
    warnings: &mut Vec<String>,
) -> Result<Option<Value>> {
    let mut entry = value
        .as_object()
        .context("Server definition must be an object")?
        .clone();
    rename(&mut entry, "protocol_version", "protocolVersion");
    for key in ["bearerToken", "bearer_token"] {
        if let Some(token) = entry.get_mut(key) {
            let token = token
                .as_str()
                .context("bearerToken must be a string; configure bearerTokenEnv instead")?;
            if token == "******" || !is_environment_reference(token) {
                bail!(
                    "Refusing to import a bearerToken credential; use bearerTokenEnv with an environment variable instead"
                );
            }
            let mut normalized = Value::String(token.to_owned());
            normalize_placeholders(&mut normalized)?;
            entry.insert(key.to_owned(), normalized);
        }
    }
    if kind.is_editor() {
        if let Some(enabled) = entry.remove("enabled")
            && !enabled.as_bool().context("enabled must be a boolean")?
        {
            warnings.push(format!("Skipped disabled {kind} server '{name}'."));
            return Ok(None);
        }
        for field in [
            "headersHelper",
            "http_headers_helper",
            "envFile",
            "oauth",
            "refresh",
            "authorization",
            "authentication",
            "inputs",
        ] {
            if entry.contains_key(field) {
                bail!("Unsupported {kind} option '{field}'; configure this server manually");
            }
        }
        for field in entry.keys() {
            let lower = field.to_ascii_lowercase();
            if [
                "auth",
                "token",
                "secret",
                "credential",
                "header",
                "password",
                "apikey",
                "api_key",
                "variable",
            ]
            .iter()
            .any(|fragment| lower.contains(fragment))
                && !matches!(
                    field.as_str(),
                    "auth"
                        | "headers"
                        | "bearerToken"
                        | "bearer_token"
                        | "bearerTokenEnv"
                        | "bearer_token_env"
                        | "tokenCacheDir"
                        | "token_cache_dir"
                        | "oauthClientId"
                        | "oauth_client_id"
                        | "oauthClientSecretEnv"
                        | "oauth_client_secret_env"
                        | "oauthClientMetadataUrl"
                        | "oauth_client_metadata_url"
                        | "oauthTokenEndpointAuthMethod"
                        | "oauth_token_endpoint_auth_method"
                        | "oauthRedirectUrl"
                        | "oauth_redirect_url"
                        | "oauthRequestedScope"
                        | "oauth_requested_scope"
                )
            {
                bail!(
                    "Unsupported {kind} credential option '{field}'; configure this server manually"
                );
            }
        }
        rename(&mut entry, "environment", "env");
        if kind == ImportKind::OpenCode {
            rename(&mut entry, "timeout", "timeoutMs");
        }
        if let Some(server_type) = entry.remove("type") {
            let server_type = server_type.as_str().context("type must be a string")?;
            let transport = match server_type {
                "local" => "stdio",
                "remote" => "http",
                "stdio" | "http" | "sse" | "streamable-http" => server_type,
                _ => bail!("Unsupported {kind} transport '{server_type}'"),
            };
            entry.entry("transport").or_insert_with(|| json!(transport));
        }
        for field in ["env", "headers"] {
            if let Some(values) = entry.get_mut(field) {
                let values = values
                    .as_object_mut()
                    .with_context(|| format!("{field} must be an object"))?;
                for value in values.values_mut() {
                    if value.is_number() || value.is_boolean() {
                        *value = json!(value.to_string());
                    }
                }
            }
        }
        if entry.get("command").is_some_and(Value::is_array)
            && let Some(arguments) = entry.remove("args")
        {
            let arguments = arguments
                .as_array()
                .context("args must be a string array")?;
            let tokens = entry
                .get_mut("command")
                .and_then(Value::as_array_mut)
                .context("command must be a string array")?;
            tokens.extend(arguments.iter().cloned());
        }
        for value in entry.values_mut() {
            normalize_placeholders(value)?;
        }
    }
    if entry.get("auth").is_some_and(|value| {
        value
            .as_str()
            .is_none_or(|method| !matches!(method, "oauth" | "none"))
    }) && kind != ImportKind::Native
    {
        bail!("Unsupported auth method; configure this server manually");
    }
    if kind == ImportKind::Codex {
        if let Some(enabled) = entry.remove("enabled")
            && !enabled.as_bool().context("enabled must be a boolean")?
        {
            warnings.push(format!("Skipped disabled Codex server '{name}'."));
            return Ok(None);
        }
        for unsupported in [
            "http_headers_helper",
            "experimental_environment",
            "tools",
            "default_tools_approval_mode",
            "oauth_client_secret",
            "oauthClientSecret",
        ] {
            if entry.contains_key(unsupported) {
                bail!(
                    "Unsupported Codex option '{unsupported}'; remove it or configure this server manually"
                );
            }
        }
        if entry
            .get("auth")
            .is_some_and(|auth| auth.as_str() != Some("oauth"))
        {
            bail!("Unsupported Codex auth method; configure this server manually");
        }
        rename(&mut entry, "http_headers", "headers");
        rename(&mut entry, "bearer_token_env_var", "bearerTokenEnv");
        let dynamic = entry.remove("env_http_headers");
        if let Some(dynamic) = dynamic {
            let dynamic = dynamic
                .as_object()
                .context("env_http_headers must be a string map")?;
            let headers = entry
                .entry("headers")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .context("http_headers must be an object")?;
            for (header, variable) in dynamic {
                let variable = variable
                    .as_str()
                    .context("env_http_headers must be a string map")?;
                headers.insert(header.clone(), json!(format!("$env:{variable}")));
            }
        }
        if let Some(variables) = entry.remove("env_vars") {
            let variables = variables
                .as_array()
                .context("env_vars must be a string array")?;
            let environment = entry
                .entry("env")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .context("env must be an object")?;
            for variable in variables {
                let variable = variable
                    .as_str()
                    .context("Only local string env_vars are supported")?;
                environment
                    .entry(variable.to_owned())
                    .or_insert_with(|| json!(format!("$env:{variable}")));
            }
        }
        let allowed = entry.remove("enabled_tools");
        let blocked = entry.remove("disabled_tools");
        if let Some(allowed) = allowed {
            let allowed = names(&allowed)?;
            let blocked = blocked.as_ref().map(names).transpose()?.unwrap_or_default();
            entry.insert(
                "allowedTools".into(),
                json!(
                    allowed
                        .into_iter()
                        .filter(|name| !blocked.contains(name))
                        .collect::<Vec<_>>()
                ),
            );
        } else if let Some(blocked) = blocked {
            entry.insert("blockedTools".into(), json!(names(&blocked)?));
        }
        if let Some(timeout) = entry.remove("tool_timeout_sec") {
            let seconds = timeout
                .as_f64()
                .context("tool_timeout_sec must be numeric")?;
            let milliseconds = seconds * 1000.0;
            if !milliseconds.is_finite() || milliseconds < 1.0 || milliseconds >= u64::MAX as f64 {
                bail!("tool_timeout_sec must be positive and within the supported range");
            }
            entry.insert("timeoutMs".into(), json!(milliseconds as u64));
        }
        for field in ["startup_timeout_sec", "required"] {
            if entry.remove(field).is_some() {
                warnings.push(format!(
                    "Server '{name}' uses '{field}', which is not applied by mcp-pool."
                ));
            }
        }
    }
    if kind == ImportKind::Claude {
        rename(&mut entry, "type", "transport");
        for field in ["headersHelper", "oauth"] {
            if entry.contains_key(field) {
                bail!("Unsupported Claude option '{field}'; configure authentication manually");
            }
        }
    }
    if kind == ImportKind::Native {
        if entry.get("auth").is_some_and(|value| !value.is_null()) {
            bail!("Legacy runtime auth cannot be imported; configure OAuth explicitly");
        }
        for field in [
            "configuration_entry",
            "auth",
            "clear_env",
            "credential_scope",
        ] {
            entry.remove(field);
        }
        rename(&mut entry, "timeout_ms", "timeoutMs");
    }
    if entry
        .get("headers")
        .and_then(Value::as_object)
        .and_then(|headers| headers.get("Authorization"))
        .and_then(Value::as_str)
        == Some("******")
    {
        bail!(
            "Refusing to import a masked Authorization header; use bearerTokenEnv with an environment variable instead"
        );
    }
    if let Some(transport) = entry.get("transport") {
        let transport = transport.as_str().context("transport must be a string")?;
        if !matches!(transport, "stdio" | "http" | "sse" | "streamable-http") {
            bail!("Unsupported transport; only stdio, http, and sse can be imported");
        }
    }
    if matches!(kind, ImportKind::Claude | ImportKind::Codex)
        && let Some(command) = entry.get("command")
    {
        let command = command
            .as_str()
            .context("command must be a string")?
            .to_owned();
        let mut tokens = vec![json!(command)];
        if let Some(arguments) = entry.remove("args") {
            let arguments = arguments
                .as_array()
                .context("args must be a string array")?;
            if arguments.iter().any(|argument| !argument.is_string()) {
                bail!("args must be a string array");
            }
            tokens.extend(arguments.iter().cloned());
        }
        entry.insert("command".into(), Value::Array(tokens));
    }
    Ok(Some(Value::Object(entry)))
}

fn is_environment_reference(value: &str) -> bool {
    value.starts_with("$env:") || value.contains("${") || value.contains("{env:")
}

fn names(value: &Value) -> Result<Vec<String>> {
    value
        .as_array()
        .context("Tool filters must be string arrays")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("Tool filters must be string arrays")
        })
        .collect()
}

fn rename(entry: &mut Map<String, Value>, old: &str, new: &str) {
    if let Some(value) = entry.remove(old) {
        entry.insert(new.into(), value);
    }
}

fn normalize_placeholders(value: &mut Value) -> Result<()> {
    match value {
        Value::String(text) => {
            for marker in [
                "{file:",
                "{input:",
                "${input:",
                "${command:",
                "${env:",
                "${workspaceFolder",
                "${config:",
                "${secret:",
            ] {
                if text.contains(marker) {
                    bail!("Unsupported variable helper '{marker}'; configure this server manually");
                }
            }
            if text.contains("{env:") {
                let mut result = String::new();
                let mut remaining = text.as_str();
                while let Some(start) = remaining.find("{env:") {
                    result.push_str(&remaining[..start]);
                    let variable = &remaining[start + 5..];
                    let end = variable
                        .find('}')
                        .context("Unclosed {env: variable helper")?;
                    let name = &variable[..end];
                    if name.is_empty()
                        || !name
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    {
                        bail!("Invalid {{env: variable helper");
                    }
                    if text.as_str() == format!("{{env:{name}}}") {
                        result.push_str(&format!("$env:{name}"));
                    } else {
                        result.push_str(&format!("${{{name}}}"));
                    }
                    remaining = &variable[end + 1..];
                }
                result.push_str(remaining);
                *text = result;
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize_placeholders(value)?;
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                normalize_placeholders(value)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn relocate_path(value: &str, directory: &Path) -> String {
    if Path::new(value).is_absolute()
        || value.starts_with(['~', '$'])
        || value.as_bytes().get(1) == Some(&b':')
        || value.starts_with(r"\\")
    {
        value.to_owned()
    } else {
        directory.join(value).to_string_lossy().into_owned()
    }
}

pub(super) fn relocate(entry: &mut Value, directory: &Path, source: &Path) -> Result<()> {
    let entry = entry.as_object_mut().context("Server must be an object")?;
    let remote = crate::server_config::is_remote_entry(entry);
    if !remote {
        let command = entry
            .get("command")
            .or_else(|| entry.get("executable"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(command) = command.filter(|command| !command.starts_with('$')) {
            let arguments = entry
                .get("args")
                .map(names)
                .transpose()?
                .unwrap_or_default();
            let tokens = if !arguments.is_empty()
                || crate::server_config::existing_command_path(&command, directory)?
            {
                let mut tokens = vec![command];
                tokens.extend(arguments);
                tokens
            } else {
                crate::server_config::command_tokens(&command)?
            };
            entry.insert("command".into(), json!(tokens));
            entry.remove("args");
            entry.remove("executable");
        }
        let cwd = entry
            .get("cwd")
            .map(|value| value.as_str().context("cwd must be a string"))
            .transpose()?;
        let cwd = cwd.map_or_else(
            || directory.to_string_lossy().into_owned(),
            |value| relocate_path(value, directory),
        );
        entry.insert("cwd".into(), json!(cwd));
        for field in ["command", "executable"] {
            if let Some(command) = entry.get_mut(field) {
                let executable = if let Value::Array(tokens) = command {
                    tokens.first_mut()
                } else {
                    Some(command)
                };
                if let Some(executable) = executable
                    && let Some(text) = executable.as_str()
                    && text.contains(['/', '\\'])
                    && !text.contains(['"', '\''])
                {
                    *executable = json!(relocate_path(text, directory));
                }
            }
        }
    }
    for field in ["tokenCacheDir", "token_cache_dir"] {
        if let Some(value) = entry.get_mut(field) {
            let text = value.as_str().context("tokenCacheDir must be a string")?;
            *value = json!(relocate_path(
                text,
                source.parent().context("Source has no parent")?
            ));
        }
    }
    Ok(())
}
