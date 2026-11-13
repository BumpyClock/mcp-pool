use super::*;
use serde_json::json;
use std::collections::HashMap;

fn old_success_response(original_id: &Value, result: &Value) -> String {
    let mut object = serde_json::Map::new();
    object.insert("jsonrpc".to_string(), Value::from("2.0"));
    object.insert("id".to_string(), original_id.clone());
    object.insert("result".to_string(), result.clone());
    Value::Object(object).to_string()
}

fn ids() -> Vec<Value> {
    vec![
        json!(0),
        json!(-42),
        json!(u64::MAX),
        json!(1.25),
        json!("plain"),
        json!("quote\" slash\\ newline\n tab\t null\0 雪 😀"),
    ]
}

fn results() -> Vec<Value> {
    vec![
        Value::Null,
        json!(true),
        json!(123.5),
        json!("quote\" slash\\ newline\n 雪 😀"),
        json!({"nested":[null, {"escaped\nkey":"value\"\\\n雪"}, [1, false]]}),
        json!({"payload":"x".repeat(256 * 1024)}),
    ]
}

fn waiter(client_id: &str, original_id: Value, now: Instant) -> PendingWaiter {
    PendingWaiter {
        client_id: client_id.to_string(),
        original_id,
        inserted_at: now,
        expires_after: Duration::from_secs(10),
    }
}

fn method_name(method: CacheableMethod) -> &'static str {
    match method {
        CacheableMethod::Initialize => "initialize",
        CacheableMethod::ServerDiscover => "server/discover",
        CacheableMethod::ToolsList => "tools/list",
    }
}

fn result_for(method: CacheableMethod) -> Value {
    match method {
        CacheableMethod::Initialize => json!({"capabilities":{}}),
        CacheableMethod::ServerDiscover => json!({"supportedVersions":["2026-07-28"]}),
        CacheableMethod::ToolsList => json!({"tools":[]}),
    }
}

#[test]
fn borrowed_success_envelope_matches_owned_map_bytes() {
    for original_id in ids() {
        for result in results() {
            let response = build_success_response(&original_id, &result);
            assert_eq!(response, old_success_response(&original_id, &result));
            assert!(response.starts_with("{\"jsonrpc\":\"2.0\",\"id\":"));
        }
    }
}

#[test]
fn retained_results_serve_different_ids_without_another_leader() -> Result<(), String> {
    for method in [CacheableMethod::Initialize, CacheableMethod::ToolsList] {
        for result in results() {
            assert_retained_hits(method, result)?;
        }
    }
    assert_retained_hits(
        CacheableMethod::ServerDiscover,
        json!({"supportedVersions":["2026-07-28"], "nested":results()}),
    )
}

fn assert_retained_hits(method: CacheableMethod, result: Value) -> Result<(), String> {
    let mut cache = HandshakeCache::default();
    let now = Instant::now();
    let mut leaders = 0;
    if matches!(
        cache.prepare(method, waiter("leader", json!("seed"), now), now),
        SharedRequestAction::Leader
    ) {
        leaders += 1;
    }
    let completed = cache.complete(method, &json!({"result":result}));
    assert!(completed.waiters.is_empty());
    for original_id in ids() {
        match cache.prepare(method, waiter("hit", original_id.clone(), now), now) {
            SharedRequestAction::Cached(response) => {
                assert_eq!(response, old_success_response(&original_id, &result));
            }
            SharedRequestAction::Leader => leaders += 1,
            SharedRequestAction::Coalesced => {
                return Err("retained result unexpectedly coalesced".to_string());
            }
        }
    }
    assert_eq!(leaders, 1);
    assert_eq!(cache.waiter_count(method), 0);
    assert!(cache.deadline(method).is_none());
    Ok(())
}

#[test]
fn completion_drains_waiters_and_invalidation_only_for_current_request() {
    for method in [CacheableMethod::ServerDiscover, CacheableMethod::ToolsList] {
        let mut cache = HandshakeCache::default();
        let now = Instant::now();
        assert!(matches!(
            cache.prepare(method, waiter("leader", json!(1), now), now),
            SharedRequestAction::Leader
        ));
        assert!(matches!(
            cache.prepare(method, waiter("follower", json!(2), now), now),
            SharedRequestAction::Coalesced
        ));
        if method == CacheableMethod::ToolsList {
            cache.invalidate_tools_list();
        }
        let first_result = match method {
            CacheableMethod::ServerDiscover => {
                json!({"supportedVersions":["2026-07-28"],"ttlMs":0})
            }
            _ => result_for(method),
        };
        let completed = cache.complete(method, &json!({"result":first_result}));
        assert_eq!(completed.waiters.len(), 1);
        assert_eq!(cache.waiter_count(method), 0);
        assert!(cache.deadline(method).is_none());
        assert!(cache.next_deadline(std::iter::empty()).is_none());
        if method == CacheableMethod::ToolsList {
            assert!(cache.get(method_name(method)).is_none());
            assert!(completed.cache_event.is_none());
        }
        assert!(
            cache
                .complete(method, &json!({"error":{"code":-32000}}))
                .waiters
                .is_empty()
        );
        cache.invalidate_tools_list();
        assert!(matches!(
            cache.prepare(method, waiter("retry", json!(3), now), now),
            SharedRequestAction::Leader
        ));
        let completed = cache.complete(method, &json!({"result":result_for(method)}));
        assert!(completed.waiters.is_empty());
        assert!(completed.cache_event.is_some());
        assert!(cache.get(method_name(method)).is_some());
    }
}

#[test]
fn follower_expiry_preserves_initialize_distinction_and_shared_deadline() -> Result<(), String> {
    for method in [
        CacheableMethod::Initialize,
        CacheableMethod::ServerDiscover,
        CacheableMethod::ToolsList,
    ] {
        let mut cache = HandshakeCache::default();
        let now = Instant::now();
        let leader_deadline = now + Duration::from_secs(20);
        let follower_deadline = now + Duration::from_secs(40);
        assert!(matches!(
            cache.prepare(method, waiter("leader", json!(1), now), leader_deadline),
            SharedRequestAction::Leader
        ));
        let shared = cache.deadline(method).ok_or("missing shared deadline")?;
        let mut requests = HashMap::from([(
            "leader-route".to_string(),
            PendingRequestInfo {
                client_id: "leader".to_string(),
                original_id: json!(1),
                method: Some(method_name(method).to_string()),
                cache_key: Some(method),
                tool: None,
                inserted_at: now,
                expires_after: Duration::from_secs(10),
            },
        )]);
        assert!(matches!(
            cache.prepare(method, waiter("follower", json!(2), now), follower_deadline),
            SharedRequestAction::Coalesced
        ));
        assert_eq!(shared.current(), follower_deadline);
        assert!(matches!(
            cache.prepare(method, waiter("short", json!(3), now), leader_deadline),
            SharedRequestAction::Coalesced
        ));
        assert_eq!(shared.current(), follower_deadline);
        cache.disconnect(&mut requests, "short");
        cache.disconnect(&mut requests, "leader");
        assert_eq!(
            requests.len(),
            1,
            "disconnected shared leader stays routable"
        );
        assert_eq!(cache.waiter_count(method), 1);
        let expected_next = if method == CacheableMethod::Initialize {
            follower_deadline
        } else {
            now + Duration::from_secs(10)
        };
        assert_eq!(cache.next_deadline(requests.values()), Some(expected_next));
        let responses = cache.expire(&mut requests, now + Duration::from_secs(11));
        if method == CacheableMethod::Initialize {
            assert!(responses.is_empty());
            assert_eq!(cache.waiter_count(method), 1);
        } else {
            assert_eq!(responses.len(), 1);
            assert_eq!(
                responses.first().map(|response| response.0.as_str()),
                Some("follower")
            );
            assert_eq!(cache.waiter_count(method), 0);
        }
        assert_eq!(requests.len(), 1);
        assert_eq!(shared.current(), follower_deadline);
        assert_eq!(
            cache
                .deadline(method)
                .ok_or("lost shared deadline")?
                .current(),
            follower_deadline
        );
        if method == CacheableMethod::ToolsList {
            cache.invalidate_tools_list();
        }
        let responses = cache.expire(&mut requests, follower_deadline);
        assert_eq!(
            responses.len(),
            if method == CacheableMethod::Initialize {
                2
            } else {
                1
            }
        );
        assert!(requests.is_empty());
        assert_eq!(cache.waiter_count(method), 0);
        assert!(cache.deadline(method).is_none());
        assert!(cache.next_deadline(requests.values()).is_none());
        assert!(cache.expire(&mut requests, follower_deadline).is_empty());
        assert!(matches!(
            cache.prepare(method, waiter("retry", json!(4), now), leader_deadline),
            SharedRequestAction::Leader
        ));
        assert!(
            cache
                .complete(method, &json!({"result":result_for(method)}))
                .waiters
                .is_empty()
        );
        assert!(cache.get(method_name(method)).is_some());
    }
    Ok(())
}
