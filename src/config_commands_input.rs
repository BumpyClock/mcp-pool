use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::{AddOperation, AddPolicy, copy};
use crate::tool_arguments::value;

pub(super) fn parse_add(
    path: Option<PathBuf>,
    root: Option<&Path>,
    mut arguments: VecDeque<String>,
) -> Result<AddOperation> {
    let mut name = None;
    let mut target = None;
    let mut entry = json!({});
    let mut command = None;
    let mut url = None;
    let mut transport = None;
    let mut passed = Vec::new();
    let mut environment = serde_json::Map::new();
    let mut headers = serde_json::Map::new();
    let mut destination = path;
    let mut dry_run = false;
    let mut copy_from = None;
    let mut from = None;
    let mut replace = false;
    while let Some(argument) = arguments.pop_front() {
        match argument.as_str() {
            "--" => {
                passed.extend(arguments);
                break;
            }
            "--command" | "--stdio" => command = Some(value(&mut arguments, &argument)?),
            "--url" => url = Some(value(&mut arguments, &argument)?),
            "--transport" => transport = Some(value(&mut arguments, &argument)?),
            "--arg" => passed.push(value(&mut arguments, &argument)?),
            "--persist" => destination = Some(PathBuf::from(value(&mut arguments, &argument)?)),
            "--from" => from = Some(PathBuf::from(value(&mut arguments, &argument)?)),
            "--replace" => replace = true,
            "--scope" => {
                let scope = value(&mut arguments, &argument)?;
                destination = Some(match scope.as_str() {
                    "home" | "user" => crate::config_paths::home_config()?,
                    "project" => crate::config_paths::project_config(root)?,
                    _ => bail!("--scope must be home or project"),
                });
            }
            "--dry-run" => dry_run = true,
            "--env" | "--header" => {
                let content = value(&mut arguments, &argument)?;
                let (key, content) = content
                    .split_once('=')
                    .or_else(|| content.split_once(':'))
                    .context("--env/--header requires KEY=value")?;
                if key.trim().is_empty() {
                    bail!("Empty environment/header name");
                }
                let values = if argument == "--env" {
                    &mut environment
                } else {
                    &mut headers
                };
                values.insert(key.trim().to_owned(), json!(content.trim()));
            }
            "--description"
            | "--auth"
            | "--token-cache-dir"
            | "--client-name"
            | "--protocol-version"
            | "--oauth-client-id"
            | "--oauth-client-secret-env"
            | "--oauth-token-endpoint-auth-method"
            | "--oauth-redirect-url"
            | "--oauth-requested-scope" => {
                let key = match argument.as_str() {
                    "--description" => "description",
                    "--auth" => "auth",
                    "--token-cache-dir" => "tokenCacheDir",
                    "--client-name" => "clientName",
                    "--protocol-version" => "protocolVersion",
                    "--oauth-client-id" => "oauthClientId",
                    "--oauth-client-secret-env" => "oauthClientSecretEnv",
                    "--oauth-token-endpoint-auth-method" => "oauthTokenEndpointAuthMethod",
                    "--oauth-redirect-url" => "oauthRedirectUrl",
                    _ => "oauthRequestedScope",
                };
                if let Some(object) = entry.as_object_mut() {
                    let value = value(&mut arguments, &argument)?;
                    if key == "protocolVersion" {
                        let preference: crate::mcp_protocol::ProtocolPreference =
                            serde_json::from_value(json!(value.clone())).map_err(|_| {
                                anyhow::anyhow!(
                                    "--protocol-version must be auto, legacy, or 2026-07-28"
                                )
                            })?;
                        let canonical = serde_json::to_value(preference)?;
                        object.insert(key.to_owned(), canonical);
                    } else {
                        object.insert(key.to_owned(), json!(value));
                    }
                }
            }
            "--copy-from" => copy_from = Some(value(&mut arguments, &argument)?),
            _ if argument.starts_with('-') => bail!("Unknown config add flag '{argument}'"),
            _ if name.is_none() => name = Some(argument),
            _ if target.is_none() => target = Some(argument),
            _ => bail!(
                "Unexpected config add argument; use --arg VALUE or -- to pass stdio arguments"
            ),
        }
    }
    let name = name.context("Usage: config add NAME [URL] [--command COMMAND]")?;
    if name.is_empty() {
        bail!("Server name cannot be empty");
    }
    if copy_from.is_none() && (from.is_some() || replace) {
        bail!("--from and --replace require --copy-from");
    }
    if let Some(target) = target {
        if command.is_some() || url.is_some() {
            bail!("Specify either a positional target or --command/--url");
        }
        if target.starts_with("http://") || target.starts_with("https://") {
            url = Some(target);
        } else {
            command = Some(target);
        }
    }
    let policy = if let Some(copy_from) = copy_from {
        copy::apply(
            &mut entry,
            &copy_from,
            from,
            root,
            command,
            url,
            transport,
            passed,
            environment,
            headers,
        )?;
        AddPolicy::Copy { replace }
    } else {
        if command.is_some() == url.is_some() {
            bail!("Specify exactly one HTTP URL or stdio command");
        }
        let kind = transport
            .as_deref()
            .unwrap_or(if url.is_some() { "http" } else { "stdio" });
        if !matches!(kind, "http" | "sse" | "stdio") || (kind == "stdio") != command.is_some() {
            bail!("--transport must agree with URL/command");
        }
        if let Some(object) = entry.as_object_mut() {
            object.insert("transport".to_owned(), json!(kind));
            if let Some(url) = url {
                object.insert("baseUrl".to_owned(), json!(url));
            }
            if let Some(command) = command {
                object.insert("command".to_owned(), json!(command));
                object.insert("args".to_owned(), json!(passed));
            }
            if !environment.is_empty() {
                object.insert("env".to_owned(), Value::Object(environment));
            }
            if !headers.is_empty() {
                object.insert("headers".to_owned(), Value::Object(headers));
            }
        }
        AddPolicy::Regular
    };
    Ok(AddOperation {
        name,
        entry,
        destination,
        dry_run,
        policy,
    })
}
