use serde_json::{Value, json};

use crate::server_config::ConfiguredServer;

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
            "allowedTools",
            "blockedTools",
        ] {
            if let Some(value) = server.raw.get(key) {
                object.insert(key.to_owned(), value.clone());
            }
        }
        if let Some(environment) = server
            .raw
            .get("env")
            .and_then(Value::as_object)
            .filter(|environment| !environment.is_empty())
        {
            object.insert(
                "env".to_owned(),
                Value::Object(
                    environment
                        .keys()
                        .map(|key| (key.clone(), json!("[redacted]")))
                        .collect(),
                ),
            );
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

pub(super) fn summary(entry: &Value) {
    println!(
        "{}",
        entry
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("server")
    );
    println!(
        "  Transport: {}",
        entry
            .get("transport")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
    );
    if let Some(description) = entry.get("description").and_then(Value::as_str) {
        println!("  Description: {description}");
    }
    if let Some(source) = entry
        .get("source")
        .and_then(|source| source.get("path"))
        .and_then(Value::as_str)
    {
        println!("  Source: local ({source})");
    }
}
