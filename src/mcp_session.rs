use std::time::{Duration, Instant};

use crate::request_deadline::SharedDeadline;
use serde_json::Value;

#[path = "session_cache.rs"]
mod cache;
pub use cache::RequestCache;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheableMethod {
    Initialize,
    ServerDiscover,
    ToolsList,
}

impl CacheableMethod {
    pub fn timeout_message(self) -> &'static str {
        match self {
            Self::Initialize => "initialize timed out",
            Self::ServerDiscover => "server/discover discovery timed out",
            Self::ToolsList => "tools/list discovery timed out",
        }
    }
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
    /// Only cacheable first-page requests share a cache entry.
    pub cache_key: Option<CacheableMethod>,
    /// Tool name for route logging only; never contains call arguments.
    pub tool: Option<String>,
    pub inserted_at: Instant,
    pub expires_after: Duration,
}

#[derive(Debug, Clone)]
pub struct PendingWaiter {
    pub client_id: String,
    pub original_id: Value,
    pub inserted_at: Instant,
    pub expires_after: Duration,
}

#[derive(Debug, Default)]
pub struct ServerDiscoveryCache {
    pub requests: RequestCache,
    pub support: DiscoverySupport,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub enum DiscoverySupport {
    #[default]
    Unknown,
    Modern,
    Legacy,
    Unsupported,
}

#[derive(Debug, Default)]
pub enum Initialization {
    #[default]
    Empty,
    InFlight {
        waiters: Vec<PendingWaiter>,
        deadline: SharedDeadline,
    },
    Ready {
        result: Value,
        notification_forwarded: bool,
    },
}

#[derive(Debug, Default)]
pub struct HandshakeCache {
    pub initialize: Initialization,
    pub server_discovery: ServerDiscoveryCache,
    pub tools_list: RequestCache,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryReason {
    SessionNotFound,
}

pub fn cacheable_method(method: &str) -> Option<CacheableMethod> {
    match method {
        "initialize" => Some(CacheableMethod::Initialize),
        "server/discover" => Some(CacheableMethod::ServerDiscover),
        "tools/list" => Some(CacheableMethod::ToolsList),
        _ => None,
    }
}

pub fn cacheable_request(value: &Value) -> Option<CacheableMethod> {
    let method = cacheable_method(value.get("method")?.as_str()?)?;
    if method == CacheableMethod::ToolsList
        && value
            .get("params")
            .and_then(|params| params.get("cursor"))
            .is_some()
    {
        return None;
    }
    Some(method)
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

pub fn tool_name(value: &Value) -> Option<String> {
    value
        .get("params")
        .and_then(|params| params.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string)
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

pub fn is_definitive_discovery_unsupported(value: &Value) -> bool {
    let Some(error) = value.get("error") else {
        return false;
    };
    error
        .get("code")
        .and_then(Value::as_i64)
        .is_some_and(|code| crate::mcp_protocol::discovery_unsupported(code, error.get("data")))
}

impl HandshakeCache {
    #[cfg(test)]
    pub fn get(&self, method: &str) -> Option<Value> {
        match cacheable_method(method) {
            Some(CacheableMethod::Initialize) => match &self.initialize {
                Initialization::Ready { result, .. } => Some(result.clone()),
                _ => None,
            },
            Some(CacheableMethod::ServerDiscover) => self.server_discovery.requests.get(),
            Some(CacheableMethod::ToolsList) => self.tools_list.get(),
            None => None,
        }
    }

    pub fn store(&mut self, method: &str, result: Value) {
        self.store_result(method, result);
    }

    pub fn store_result(&mut self, method: &str, result: Value) {
        match cacheable_method(method) {
            Some(CacheableMethod::Initialize) => {
                self.initialize = Initialization::Ready {
                    result,
                    notification_forwarded: false,
                }
            }
            Some(CacheableMethod::ServerDiscover) => {
                if let Ok(modern_supported) = crate::mcp_protocol::discovery_offers_modern(&result)
                {
                    self.server_discovery.support = if modern_supported {
                        DiscoverySupport::Modern
                    } else {
                        DiscoverySupport::Legacy
                    };
                }
                self.server_discovery.requests.store(result);
            }
            Some(CacheableMethod::ToolsList) => {
                self.tools_list.store(result);
            }
            None => {}
        }
    }

    pub fn invalidate_tools_list(&mut self) {
        self.tools_list.invalidate();
    }

    pub fn clear_all(&mut self) {
        *self = Self::default();
    }

    pub fn swallow_initialized(&mut self, value: &Value) -> bool {
        if value.get("method").and_then(Value::as_str) != Some("notifications/initialized")
            || !value.get("id").is_none_or(Value::is_null)
        {
            return false;
        }
        match &mut self.initialize {
            Initialization::Ready {
                notification_forwarded,
                ..
            } => {
                let duplicate = *notification_forwarded;
                *notification_forwarded = true;
                duplicate
            }
            _ => false,
        }
    }

    pub fn deadline(&self, method: CacheableMethod) -> Option<SharedDeadline> {
        match method {
            CacheableMethod::Initialize => match &self.initialize {
                Initialization::InFlight { deadline, .. } => Some(deadline.clone()),
                _ => None,
            },
            CacheableMethod::ServerDiscover => self.server_discovery.requests.in_flight.clone(),
            CacheableMethod::ToolsList => self.tools_list.in_flight.clone(),
        }
    }

    pub fn clear_expired_results(&mut self) {
        self.server_discovery.requests.clear_expired();
        self.tools_list.clear_expired();
    }
}

fn cache_deadline(result: &Value) -> Option<Instant> {
    if result
        .get("cacheScope")
        .and_then(Value::as_str)
        .is_some_and(|scope| scope != "public")
    {
        // This generation has no authorization partition, so private results
        // cannot be retained in its shared cache.
        return Some(Instant::now());
    }
    let ttl = result.get("ttlMs").and_then(Value::as_u64);
    match ttl {
        Some(0) => Some(Instant::now()),
        Some(milliseconds) => Instant::now().checked_add(Duration::from_millis(milliseconds)),
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tool_name_reads_params_name_for_tools_call() {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {"name": "ListCalendarView", "arguments": {"secret": "x"}}
        });
        assert_eq!(tool_name(&request).as_deref(), Some("ListCalendarView"));
    }

    #[test]
    fn tool_name_is_none_without_params_name() {
        let no_name = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {}});
        let no_params = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"});
        assert_eq!(tool_name(&no_name), None);
        assert_eq!(tool_name(&no_params), None);
    }

    #[test]
    fn cacheable_method_recognizes_handshake_methods_only() {
        assert_eq!(
            cacheable_method("initialize"),
            Some(CacheableMethod::Initialize)
        );
        assert_eq!(
            cacheable_method("server/discover"),
            Some(CacheableMethod::ServerDiscover)
        );
        assert_eq!(
            cacheable_method("tools/list"),
            Some(CacheableMethod::ToolsList)
        );
        assert_eq!(cacheable_method("tools/call"), None);
        assert_eq!(cacheable_method("notifications/initialized"), None);
    }

    #[test]
    fn tools_list_cursor_requests_are_not_shared_discovery() {
        assert_eq!(
            cacheable_request(&json!({"method":"tools/list"})),
            Some(CacheableMethod::ToolsList)
        );
        assert_eq!(
            cacheable_request(&json!({"method":"tools/list","params":{}})),
            Some(CacheableMethod::ToolsList)
        );
        assert_eq!(
            cacheable_request(&json!({"method":"tools/list","params":{"cursor":"page-two"}})),
            None
        );
        assert_eq!(
            cacheable_request(&json!({"method":"tools/list","params":{"cursor":null}})),
            None
        );
        assert_eq!(
            cacheable_request(&json!({"method":"initialize"})),
            Some(CacheableMethod::Initialize)
        );
        assert_eq!(
            cacheable_request(&json!({"method":"server/discover","params":{}})),
            Some(CacheableMethod::ServerDiscover)
        );
        assert_eq!(cacheable_request(&json!({"method":"tools/call"})), None);
    }

    #[test]
    fn explicit_cache_ttls_control_tools_and_discovery_retention() {
        let mut cache = HandshakeCache::default();
        cache.store("tools/list", json!({"tools":[],"ttlMs":0}));
        cache.store(
            "server/discover",
            json!({"supportedVersions":["2025-11-25"],"ttlMs":0}),
        );
        assert!(cache.get("tools/list").is_none());
        assert!(cache.get("server/discover").is_none());

        cache.store("tools/list", json!({"tools":[],"ttlMs":1}));
        cache.store(
            "server/discover",
            json!({"supportedVersions":["2025-11-25"],"ttlMs":1}),
        );
        cache.tools_list.cached_until = Some(Instant::now() - Duration::from_secs(1));
        cache.server_discovery.requests.cached_until =
            Some(Instant::now() - Duration::from_secs(1));
        assert!(cache.get("tools/list").is_none());
        assert!(cache.get("server/discover").is_none());

        cache.store(
            "tools/list",
            json!({"tools":[],"ttlMs":1000,"cacheScope":"private"}),
        );
        assert!(cache.get("tools/list").is_none());
        cache.store(
            "tools/list",
            json!({"tools":[],"ttlMs":1000,"cacheScope":"public"}),
        );
        assert!(cache.get("tools/list").is_some());
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
    fn discovery_unsupported_only_accepts_definitive_error_shapes() {
        assert!(is_definitive_discovery_unsupported(
            &json!({"error":{"code":-32601,"message":"Method not found"}})
        ));
        for status in [400, 404, 405, 415] {
            assert!(is_definitive_discovery_unsupported(&json!({
                "error":{"code":-32004,"data":{"httpStatus":status}}
            })));
        }
        for value in [
            json!({"error":{"code":-32004,"data":{"httpStatus":401}}}),
            json!({"error":{"code":-32000,"data":{"httpStatus":404}}}),
            json!({"error":{"code":-32602}}),
        ] {
            assert!(!is_definitive_discovery_unsupported(&value));
        }
    }

    #[test]
    fn cached_initialize_lifecycle_swallows_initialized_notification() {
        let mut cache = HandshakeCache::default();
        cache.store("initialize", json!({"capabilities":{}}));
        let initialized = json!({"jsonrpc":"2.0","method":"notifications/initialized"});
        assert!(!cache.swallow_initialized(&initialized));
        assert!(cache.swallow_initialized(&initialized));
        assert!(!cache.swallow_initialized(&json!({"method":"notifications/progress"})));
    }
}
