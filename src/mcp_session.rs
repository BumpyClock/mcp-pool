use std::time::Instant;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheableMethod {
    Initialize,
    ToolsList,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClientCapabilities {
    pub sampling: bool,
    pub roots: bool,
}

#[derive(Debug, Clone)]
pub struct PendingRequestInfo {
    pub client_id: String,
    pub original_id: Value,
    pub method: Option<String>,
    pub inserted_at: Instant,
}

#[derive(Debug, Clone)]
pub struct PendingWaiter {
    pub client_id: String,
    pub original_id: Value,
    pub inserted_at: Instant,
}

#[derive(Debug, Default)]
pub struct ToolsListCache {
    pub cached_result: Option<Value>,
    pub last_good_result: Option<Value>,
    pub waiters: Vec<PendingWaiter>,
    pub in_flight: bool,
}

#[derive(Debug, Default)]
pub struct HandshakeCache {
    pub initialize: Option<Value>,
    pub tools_list: ToolsListCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryReason {
    SessionNotFound,
}

pub fn cacheable_method(method: &str) -> Option<CacheableMethod> {
    match method {
        "initialize" => Some(CacheableMethod::Initialize),
        "tools/list" => Some(CacheableMethod::ToolsList),
        _ => None,
    }
}

pub fn build_success_response(original_id: Value, result: Value) -> String {
    let mut object = serde_json::Map::new();
    object.insert("jsonrpc".to_string(), Value::from("2.0"));
    object.insert("id".to_string(), original_id);
    object.insert("result".to_string(), result);
    Value::Object(object).to_string()
}

pub fn build_error_response(original_id: Value, code: i64, message: &str) -> String {
    let mut error = serde_json::Map::new();
    error.insert("code".to_string(), Value::from(code));
    error.insert("message".to_string(), Value::from(message));

    let mut object = serde_json::Map::new();
    object.insert("jsonrpc".to_string(), Value::from("2.0"));
    object.insert("id".to_string(), original_id);
    object.insert("error".to_string(), Value::Object(error));
    Value::Object(object).to_string()
}

pub fn parse_client_capabilities(initialize_request: &Value) -> ClientCapabilities {
    let capabilities = initialize_request
        .get("params")
        .and_then(|params| params.get("capabilities"));

    ClientCapabilities {
        sampling: capabilities
            .and_then(|capabilities| capabilities.get("sampling"))
            .is_some_and(Value::is_object),
        roots: capabilities
            .and_then(|capabilities| capabilities.get("roots"))
            .is_some_and(Value::is_object),
    }
}

pub fn is_session_not_found_error(value: &Value) -> bool {
    let Some(error) = value.get("error") else {
        return false;
    };
    let code_matches = error.get("code").and_then(Value::as_i64) == Some(-32001);
    let message_matches = error
        .get("message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("Session not found"));
    code_matches && message_matches
}

pub fn is_empty_id_error(value: &Value) -> bool {
    value.get("method").is_none()
        && value.get("error").is_some()
        && matches!(value.get("id"), Some(Value::String(id)) if id.is_empty())
}

pub fn should_swallow_initialized(locally_initialized: bool, value: &Value) -> bool {
    locally_initialized
        && value.get("method").and_then(Value::as_str) == Some("notifications/initialized")
        && value.get("id").is_none_or(Value::is_null)
}

impl HandshakeCache {
    pub fn get(&self, method: &str) -> Option<Value> {
        match cacheable_method(method) {
            Some(CacheableMethod::Initialize) => self.initialize.clone(),
            Some(CacheableMethod::ToolsList) => self.tools_list.cached_result.clone(),
            None => None,
        }
    }

    pub fn store(&mut self, method: &str, result: Value) {
        match cacheable_method(method) {
            Some(CacheableMethod::Initialize) => self.initialize = Some(result),
            Some(CacheableMethod::ToolsList) => {
                self.tools_list.cached_result = Some(result.clone());
                self.tools_list.last_good_result = Some(result);
            }
            None => {}
        }
    }

    pub fn invalidate_tools_list(&mut self) {
        self.tools_list.cached_result = None;
    }

    pub fn clear_all(&mut self) {
        self.initialize = None;
        self.tools_list.cached_result = None;
        self.tools_list.waiters.clear();
        self.tools_list.in_flight = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cacheable_method_recognizes_handshake_methods_only() {
        assert_eq!(
            cacheable_method("initialize"),
            Some(CacheableMethod::Initialize)
        );
        assert_eq!(
            cacheable_method("tools/list"),
            Some(CacheableMethod::ToolsList)
        );
        assert_eq!(cacheable_method("tools/call"), None);
        assert_eq!(cacheable_method("notifications/initialized"), None);
    }

    #[test]
    fn parse_client_capabilities_reads_sampling_and_roots() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "capabilities": {
                    "sampling": {},
                    "roots": {"listChanged": true}
                }
            }
        });

        let capabilities = parse_client_capabilities(&request);

        assert!(capabilities.sampling);
        assert!(capabilities.roots);
    }

    #[test]
    fn session_not_found_requires_code_and_message() {
        let error = json!({
            "jsonrpc": "2.0",
            "id": "",
            "error": {"code": -32001, "message": "Session not found"}
        });
        let wrong_code = json!({
            "jsonrpc": "2.0",
            "id": "",
            "error": {"code": -32002, "message": "Session not found"}
        });
        let wrong_message = json!({
            "jsonrpc": "2.0",
            "id": "",
            "error": {"code": -32001, "message": "Other failure"}
        });

        assert!(is_session_not_found_error(&error));
        assert!(!is_session_not_found_error(&wrong_code));
        assert!(!is_session_not_found_error(&wrong_message));
    }

    #[test]
    fn empty_id_error_requires_error_and_empty_string_id() {
        assert!(is_empty_id_error(&json!({
            "jsonrpc": "2.0",
            "id": "",
            "error": {"code": -32001, "message": "Session not found"}
        })));
        assert!(!is_empty_id_error(&json!({
            "jsonrpc": "2.0",
            "id": "",
            "result": {}
        })));
        assert!(!is_empty_id_error(&json!({
            "jsonrpc": "2.0",
            "error": {"code": -32001, "message": "Session not found"}
        })));
    }

    #[test]
    fn cached_initialize_lifecycle_swallows_initialized_notification() {
        assert!(should_swallow_initialized(
            true,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        ));
        assert!(!should_swallow_initialized(
            false,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
        ));
        assert!(!should_swallow_initialized(
            true,
            &json!({"jsonrpc": "2.0", "method": "notifications/progress"})
        ));
    }
}
