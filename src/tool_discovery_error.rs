use serde_json::{Value, json};

pub(super) fn attach_error(entry: &mut Value, name: &str, error: &str) {
    let status = if error.starts_with("Tool '") {
        "error"
    } else {
        category(error)
    };
    if let Some(object) = entry.as_object_mut() {
        object.insert("status".to_owned(), json!(status));
        object.insert("error".to_owned(), json!(error));
        object.insert("issue".to_owned(), issue(error));
        if status == "auth" {
            object.insert(
                "authCommand".to_owned(),
                json!(format!("mcp-pool auth {name}")),
            );
        }
    }
}

pub(crate) fn category(error: &str) -> &'static str {
    match issue(error).get("kind").and_then(Value::as_str) {
        Some("auth") => "auth",
        Some("offline") => "offline",
        Some("http") => "http",
        _ => "error",
    }
}

pub(crate) fn issue(error: &str) -> Value {
    let lower = error.to_ascii_lowercase();
    let tokens: Vec<&str> = lower
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .collect();
    let status = if lower.contains("http") || lower.contains("status") {
        tokens
            .iter()
            .filter_map(|token| token.parse::<u16>().ok())
            .find(|status| (400..600).contains(status))
    } else {
        None
    };
    let kind = if status.is_some_and(|status| matches!(status, 401 | 403))
        || lower.contains("oauth")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("invalid_token")
    {
        "auth"
    } else if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("deadline")
        || lower.contains("connection refused")
        || lower.contains("failed to start")
        || lower.contains("no such file")
        || lower.contains("system cannot find")
        || lower.contains("closed")
        || lower.contains("connection reset")
        || lower.contains("fetch failed")
        || lower.contains("network is unreachable")
    {
        "offline"
    } else if status.is_some() {
        "http"
    } else if tokens.contains(&"401") {
        "auth"
    } else {
        "other"
    };
    let mut issue = json!({"kind":kind,"rawMessage":error});
    if let Some(status) = status
        && let Some(object) = issue.as_object_mut()
    {
        object.insert("statusCode".to_owned(), json!(status));
    }
    issue
}
