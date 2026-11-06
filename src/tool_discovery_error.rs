use std::borrow::Cow;

use serde_json::{Value, json};

use crate::mcp_client::McpRpcError;
use crate::mcp_protocol::{FailureKind, TransportFailure};

#[derive(Debug)]
pub(crate) struct Failure {
    raw: String,
    metadata: Option<TransportFailure>,
}

impl Failure {
    pub(crate) fn discovery(error: &anyhow::Error, raw: String) -> Self {
        if error
            .downcast_ref::<crate::tool_filter::ToolSelectionError>()
            .is_some()
        {
            return Self::selection(raw);
        }
        let metadata = error
            .downcast_ref::<McpRpcError>()
            .and_then(|error| error.data.as_ref())
            .and_then(TransportFailure::from_data);
        Self { raw, metadata }
    }

    pub(super) fn selection(raw: String) -> Self {
        Self {
            raw,
            metadata: Some(TransportFailure {
                kind: FailureKind::Error,
                status_code: None,
            }),
        }
    }

    pub(super) fn timeout(raw: String) -> Self {
        Self {
            raw,
            metadata: Some(TransportFailure {
                kind: FailureKind::Timeout,
                status_code: None,
            }),
        }
    }

    pub(super) fn raw(&self) -> &str {
        &self.raw
    }

    pub(crate) fn issue(&self) -> Value {
        match self.metadata {
            Some(metadata) => {
                let mut issue = json!({"kind":metadata.kind,"rawMessage":self.raw});
                if let Some(status) = metadata.status_code
                    && let Some(object) = issue.as_object_mut()
                {
                    object.insert("statusCode".to_owned(), json!(status));
                }
                issue
            }
            None => issue(&self.raw),
        }
    }

    pub(super) fn status(&self) -> &'static str {
        match self.issue().get("kind").and_then(Value::as_str) {
            Some("auth") => "auth",
            Some("offline" | "timeout") => "offline",
            Some("http") => "http",
            _ => "error",
        }
    }

    pub(super) fn status_code(&self) -> Option<u64> {
        self.issue().get("statusCode").and_then(Value::as_u64)
    }
}

pub(super) fn attach_error(entry: &mut Value, name: &str, error: &Failure) {
    let status = error.status();
    if let Some(object) = entry.as_object_mut() {
        object.insert("status".to_owned(), json!(status));
        object.insert("error".to_owned(), json!(user_message(name, error)));
        object.insert("issue".to_owned(), error.issue());
        if status == "auth" {
            object.insert(
                "authCommand".to_owned(),
                json!(format!("mcp-cli auth {name}")),
            );
        }
    }
}

pub(super) fn user_message<'a>(name: &str, error: &'a Failure) -> Cow<'a, str> {
    match error.status_code() {
        Some(401) => Cow::Owned(format!(
            "HTTP 401 Unauthorized. Check configured credentials. For OAuth, run `mcp-cli auth {name}`."
        )),
        Some(403) => Cow::Owned(format!(
            "HTTP 403 Forbidden. Check account permissions and configured credentials. For OAuth, run `mcp-cli auth {name}`."
        )),
        _ => Cow::Borrowed(error.raw()),
    }
}

#[cfg(test)]
pub(super) fn category(error: &str) -> &'static str {
    Failure {
        raw: error.to_owned(),
        metadata: None,
    }
    .status()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_auth_failures_offer_recovery_and_preserve_diagnostics() {
        for status in [401, 403] {
            let raw = format!(
                "MCP protocol discovery failed: JSON-RPC error -32000: HTTP upstream returned status {status}; request was not replayed"
            );
            let mut entry = json!({});
            let failure = Failure {
                raw: raw.clone(),
                metadata: None,
            };
            attach_error(&mut entry, "docs", &failure);
            let message = user_message("docs", &failure);
            assert!(message.contains(&format!("HTTP {status}")));
            assert!(message.contains("configured credentials"));
            assert!(message.contains("`mcp-cli auth docs`"));
            assert!(!message.contains("protocolVersion"));
            if status == 403 {
                assert!(message.contains("permissions"));
            }
            assert_eq!(entry.get("error"), Some(&json!(message)));
            assert_eq!(entry.pointer("/issue/rawMessage"), Some(&json!(raw)));
            assert_eq!(entry.pointer("/issue/statusCode"), Some(&json!(status)));
            assert_eq!(entry.get("status"), Some(&json!("auth")));
            assert_eq!(entry.get("authCommand"), Some(&json!("mcp-cli auth docs")));
        }
    }

    #[test]
    fn other_failures_keep_their_original_messages() {
        for raw in [
            "HTTP upstream returned status 503; request was not replayed",
            "MCP tools/list deadline exceeded",
            "OAuth credentials missing or expired; run `mcp-cli auth docs` explicitly",
        ] {
            let mut entry = json!({});
            let failure = Failure {
                raw: raw.to_owned(),
                metadata: None,
            };
            attach_error(&mut entry, "docs", &failure);
            assert_eq!(user_message("docs", &failure), raw);
            assert_eq!(entry.get("error"), Some(&json!(raw)));
            assert_eq!(entry.pointer("/issue/rawMessage"), Some(&json!(raw)));
        }
    }

    #[test]
    fn structured_failures_override_diagnostic_words_and_preserve_raw_messages() {
        for (kind, status_code, status) in [
            (FailureKind::Error, None, "error"),
            (FailureKind::Offline, None, "offline"),
            (FailureKind::Timeout, None, "offline"),
            (FailureKind::Http, Some(503), "http"),
            (FailureKind::Auth, Some(401), "auth"),
            (FailureKind::Auth, Some(403), "auth"),
        ] {
            let raw = "backend oauth forbidden timeout HTTP 401 diagnostic";
            let rpc = McpRpcError {
                code: -32000,
                message: raw.to_owned(),
                data: Some(TransportFailure { kind, status_code }.data()),
            };
            let error = anyhow::Error::new(rpc).context("MCP discovery failed");
            let failure = Failure::discovery(&error, raw.to_owned());
            let mut entry = json!({});
            attach_error(&mut entry, "docs", &failure);
            assert_eq!(entry.get("status"), Some(&json!(status)));
            assert_eq!(entry.pointer("/issue/kind"), Some(&json!(kind)));
            assert_eq!(entry.pointer("/issue/rawMessage"), Some(&json!(raw)));
            assert_eq!(
                entry.pointer("/issue/statusCode").and_then(Value::as_u64),
                status_code.map(u64::from)
            );
            if kind == FailureKind::Auth {
                assert!(user_message("docs", &failure).contains("configured credentials"));
                assert_eq!(entry.get("authCommand"), Some(&json!("mcp-cli auth docs")));
            } else {
                assert_eq!(user_message("docs", &failure), raw);
                assert!(entry.get("authCommand").is_none());
            }
        }
    }

    #[test]
    fn typed_selection_failures_never_classify_names_as_transport_failures() {
        for name in ["oauth", "timeout", "forbidden", "HTTP 401"] {
            let raw = format!("Unknown tool '{name}' on 'docs'");
            let error = anyhow::Error::new(crate::tool_filter::ToolSelectionError(raw.clone()));
            let failure = Failure::discovery(&error, raw.clone());
            assert_eq!(failure.status(), "error");
            assert_eq!(failure.issue(), json!({"kind":"error","rawMessage":raw}));
            assert_eq!(user_message("docs", &failure), raw);
        }
    }
}
