use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

/// Allocates request IDs shared by all clients of one upstream.
#[derive(Debug)]
pub struct IdAllocator {
    next: AtomicU64,
}

impl IdAllocator {
    pub fn new() -> Self {
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

/// Keeps numeric and string JSON-RPC IDs distinct in routing maps.
pub fn id_key(id: &Value) -> String {
    id.to_string()
}

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
