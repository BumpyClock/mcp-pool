use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// Monotonic source of pool-unique JSON-RPC request ids.
///
/// Each pooled upstream multiplexes many clients onto one connection. Clients
/// independently number their requests (1, 2, 3, ...), so raw ids collide across
/// clients. The pool rewrites every client request id to a unique pool id before
/// forwarding and restores the client's original id on the matching response.
#[derive(Debug)]
pub struct IdAllocator {
    next: AtomicU64,
}

impl IdAllocator {
    pub fn new() -> Self {
        // Start at 1: id 0 is legal JSON-RPC but starting at 1 keeps logs and any
        // id-sensitive upstream tooling conventional.
        Self {
            next: AtomicU64::new(1),
        }
    }

    pub fn allocate(&self) -> u64 {
        self.next.fetch_add(1, Ordering::Relaxed)
    }
}

impl Default for IdAllocator {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical request-map key for a JSON-RPC id. Uses the serialized form so a
/// numeric id (`1` -> `"1"`) and a string id (`"1"` -> `"\"1\""`) never collide.
pub fn id_key(id: &Value) -> String {
    id.to_string()
}

/// Rewrite an object message's `id` field, returning the serialized line. Falls
/// back to a clone-free re-serialization of the whole object. Panic-free: only
/// mutates when the message is an object (every JSON-RPC frame is).
pub fn with_id(mut object: serde_json::Map<String, Value>, new_id: Value) -> String {
    object.insert("id".to_string(), new_id);
    Value::Object(object).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn allocator_is_monotonic_and_unique() {
        let allocator = IdAllocator::new();
        let first = allocator.allocate();
        let second = allocator.allocate();
        let third = allocator.allocate();
        assert_eq!(first, 1);
        assert_eq!(second, 2);
        assert_eq!(third, 3);
    }

    #[test]
    fn id_key_separates_number_and_string() {
        assert_eq!(id_key(&json!(1)), "1");
        assert_eq!(id_key(&json!("1")), "\"1\"");
        assert_ne!(id_key(&json!(1)), id_key(&json!("1")));
    }

    #[test]
    fn with_id_replaces_and_round_trips() {
        let object = json!({"jsonrpc": "2.0", "id": 5, "method": "tools/list"})
            .as_object()
            .cloned()
            .expect("object");
        let line = with_id(object, Value::from(42u64));
        let parsed: Value = serde_json::from_str(&line).expect("valid json");
        assert_eq!(parsed.get("id"), Some(&json!(42)));
        assert_eq!(parsed.get("method"), Some(&json!("tools/list")));
    }
}
