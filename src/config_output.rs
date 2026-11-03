use serde_json::{Value, json};

use crate::server_config::ConfiguredServer;

#[cfg(test)]
#[path = "config_output_tests.rs"]
mod tests;

pub(crate) fn serialize(server: &ConfiguredServer) -> Value {
    let definition = &server.definition;
    let mut entry = json!({"name":server.name,"source":{"kind":"local","path":server.source},
        "transport":definition.transport_kind()});
    if let Some(object) = entry.as_object_mut() {
        if !definition.description.is_empty() {
            object.insert("description".to_owned(), json!(definition.description));
        }
        for key in [
            "auth",
            "clientName",
            "tokenCacheDir",
            "oauthClientId",
            "oauthClientSecretEnv",
            "oauthTokenEndpointAuthMethod",
            "oauthRedirectUrl",
            "oauthRequestedScope",
            "lifecycle",
        ] {
            if let Some(value) = server.raw.get(key) {
                object.insert(key.to_owned(), value.clone());
            }
        }
        if let Some(value) = server
            .raw
            .get("protocolVersion")
            .or_else(|| server.raw.get("protocol_version"))
        {
            object.insert("protocolVersion".to_owned(), value.clone());
        }
        for (key, alias) in [
            ("allowedTools", "allowed_tools"),
            ("blockedTools", "blocked_tools"),
        ] {
            if let Some(value) = server.raw.get(key).or_else(|| server.raw.get(alias)) {
                object.insert(key.to_owned(), value.clone());
            }
        }
        let environment = server.configured_environment();
        let environment: serde_json::Map<String, Value> = environment
            .keys()
            .map(|key| (key.to_owned(), json!("[redacted]")))
            .collect();
        if !environment.is_empty() {
            object.insert("env".to_owned(), Value::Object(environment));
        }
        if definition.is_remote() {
            object.insert(
                "baseUrl".to_owned(),
                json!(crate::mcp_cli::display_url(&definition.url)),
            );
            if !definition.headers.is_empty() {
                object.insert(
                    "headers".to_owned(),
                    Value::Object(
                        definition
                            .headers
                            .keys()
                            .map(|key| (key.clone(), json!("[redacted]")))
                            .collect(),
                    ),
                );
            }
        } else {
            object.insert("command".to_owned(), json!(definition.command));
            object.insert("args".to_owned(), json!(definition.args));
            if let Some(cwd) = &definition.cwd {
                object.insert("cwd".to_owned(), json!(cwd));
            }
        }
    }
    entry
}

pub(super) fn credential_summary(
    name: &str,
    oauth_configured: bool,
    credentials: &Value,
) -> String {
    let name = name.escape_debug();
    let flag = |key| credentials.get(key).and_then(Value::as_bool) == Some(true);
    let refreshable = flag("refreshable") && flag("hasClientRegistration");
    let status = if !flag("bindingValid") {
        "credential binding mismatch"
    } else if flag("authenticated") {
        "cached OAuth credentials valid"
    } else if flag("expired") && refreshable {
        "cached OAuth credentials expired (refresh available on use)"
    } else if flag("expired") {
        "cached OAuth credentials expired"
    } else if oauth_configured {
        "no usable cached OAuth credentials"
    } else {
        "OAuth not configured (no usable cached credentials)"
    };
    let mut report = format!("  {name}: {status}");
    if !flag("bindingValid")
        || (flag("expired") && !refreshable)
        || (oauth_configured && !flag("hasTokens"))
    {
        report.push_str(&format!(
            "\n    Run mcp-pool auth {name} to authorize explicitly."
        ));
    }
    report
}

pub(super) fn summary(server: &ConfiguredServer) {
    let entry = serialize(server);
    println!("{}", server.name.escape_debug());
    println!("  Transport: {}", server.definition.transport_kind());
    if let Some(command) = entry.get("command").and_then(Value::as_str) {
        println!("  Command: {}", command.escape_debug());
        if let Some(arguments) = entry.get("args").and_then(Value::as_array)
            && !arguments.is_empty()
        {
            let arguments = arguments
                .iter()
                .filter_map(Value::as_str)
                .map(|argument| format!("{argument:?}"))
                .collect::<Vec<_>>()
                .join(" ");
            println!("  Args: {arguments}");
        }
    }
    for (key, label) in [
        ("baseUrl", "URL"),
        ("cwd", "CWD"),
        ("description", "Description"),
        ("auth", "Auth"),
        ("protocolVersion", "Protocol"),
    ] {
        if let Some(value) = entry.get(key).and_then(Value::as_str) {
            println!("  {label}: {}", value.escape_debug());
        }
    }
    if let Some(keep_alive) = server.configured_lifecycle() {
        println!(
            "  Lifecycle: {}",
            if keep_alive {
                "keep-alive"
            } else {
                "ephemeral"
            }
        );
    }
    for (key, label) in [("env", "Environment"), ("headers", "Headers")] {
        if let Some(values) = entry.get(key).and_then(Value::as_object) {
            let names = values
                .keys()
                .map(|name| name.escape_debug().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            println!("  {label}: {names} (values redacted)");
        }
    }
    if let Some(source) = entry
        .get("source")
        .and_then(|source| source.get("path"))
        .and_then(Value::as_str)
    {
        println!("  Source: local ({})", source.escape_debug());
    }
}
