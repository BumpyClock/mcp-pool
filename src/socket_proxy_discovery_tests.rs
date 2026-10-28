use super::initialize_tests::{Fixture, connect, read, send};
use super::*;
use serde_json::json;
use tokio::io::BufReader;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_server_discovery_has_one_upstream_request_and_correlated_waiters()
-> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = BufReader::new(connect(&fixture.proxy).await?);
    let mut second = BufReader::new(connect(&fixture.proxy).await?);
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"first","method":"server/discover","params":{}}),
    )
    .await?;
    let leader = fixture.request().await?;
    assert_eq!(leader.get("method"), Some(&json!("server/discover")));
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":"second","method":"server/discover","params":{}}),
    )
    .await?;

    let generation = fixture
        .proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while generation
            .handshake_cache
            .lock()
            .waiter_count(CacheableMethod::ServerDiscover)
            != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    assert!(fixture.requests.try_recv().is_err());

    let result = json!({
        "supportedVersions":["2026-07-28"],
        "capabilities":{"tools":{}},
        "ttlMs":1000
    });
    fixture
        .responses
        .send(json!({"jsonrpc":"2.0","id":leader.get("id"),"result":result}).to_string())
        .await
        .map_err(io::Error::other)?;
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":"first","result":result})
    );
    assert_eq!(
        read(&mut second).await?,
        json!({"jsonrpc":"2.0","id":"second","result":result})
    );

    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"cached","method":"server/discover","params":{}}),
    )
    .await?;
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":"cached","result":result})
    );
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}

#[tokio::test]
async fn discovery_error_reaches_all_waiters_and_allows_legacy_fallback() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut first = BufReader::new(connect(&fixture.proxy).await?);
    let mut second = BufReader::new(connect(&fixture.proxy).await?);
    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"discover-a","method":"server/discover","params":{}}),
    )
    .await?;
    let leader = fixture.request().await?;
    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":"discover-b","method":"server/discover","params":{}}),
    )
    .await?;
    let generation = fixture
        .proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while generation
            .handshake_cache
            .lock()
            .waiter_count(CacheableMethod::ServerDiscover)
            != 1
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(io::Error::other)?;
    let error = json!({"code":-32601,"message":"Method not found"});
    fixture
        .responses
        .send(json!({"jsonrpc":"2.0","id":leader.get("id"),"error":error}).to_string())
        .await
        .map_err(io::Error::other)?;
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":"discover-a","error":error})
    );
    assert_eq!(
        read(&mut second).await?,
        json!({"jsonrpc":"2.0","id":"discover-b","error":error})
    );

    send(
        second.get_mut(),
        json!({"jsonrpc":"2.0","id":"discover-c","method":"server/discover","params":{}}),
    )
    .await?;
    assert_eq!(
        read(&mut second).await?,
        json!({"jsonrpc":"2.0","id":"discover-c","error":{
            "code":-32601,
            "message":"server/discover is unsupported by this upstream; use legacy initialize"
        }})
    );
    assert!(fixture.requests.try_recv().is_err());

    send(
        first.get_mut(),
        json!({"jsonrpc":"2.0","id":"legacy-a","method":"initialize","params":{}}),
    )
    .await?;
    let initialize = fixture.request().await?;
    assert_eq!(initialize.get("method"), Some(&json!("initialize")));
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":initialize.get("id"),"result":{
                "protocolVersion":"2025-11-25","capabilities":{}
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(
        read(&mut first).await?,
        json!({"jsonrpc":"2.0","id":"legacy-a","result":{
            "protocolVersion":"2025-11-25","capabilities":{}
        }})
    );
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}

#[tokio::test]
async fn legacy_discovery_offer_does_not_block_legacy_initialize() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut client = BufReader::new(connect(&fixture.proxy).await?);
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"discover","method":"server/discover","params":{}}),
    )
    .await?;
    let discovery = fixture.request().await?;
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":discovery.get("id"),"result":{
                "supportedVersions":["2025-11-25"],"capabilities":{}
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut client).await?.get("id"), Some(&json!("discover")));

    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"initialize","method":"initialize","params":{}}),
    )
    .await?;
    let initialize = fixture.request().await?;
    assert_eq!(initialize.get("method"), Some(&json!("initialize")));
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":initialize.get("id"),"result":{
                "protocolVersion":"2025-11-25","capabilities":{}
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(
        read(&mut client).await?.get("id"),
        Some(&json!("initialize"))
    );
    fixture.stop().await
}

#[tokio::test]
async fn malformed_discovery_result_is_forwarded_and_not_success_cached() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut client = BufReader::new(connect(&fixture.proxy).await?);
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"malformed","method":"server/discover","params":{}}),
    )
    .await?;
    let malformed = fixture.request().await?;
    fixture
        .responses
        .send(json!({"jsonrpc":"2.0","id":malformed.get("id"),"result":{}}).to_string())
        .await
        .map_err(io::Error::other)?;
    assert_eq!(
        read(&mut client).await?,
        json!({"jsonrpc":"2.0","id":"malformed","result":{}})
    );

    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"retry","method":"server/discover","params":{}}),
    )
    .await?;
    let retry = fixture.request().await?;
    assert_ne!(retry.get("id"), malformed.get("id"));
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":retry.get("id"),"error":{
                "code":-32601,"message":"Method not found"
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut client).await?.get("id"), Some(&json!("retry")));
    fixture.stop().await
}

#[tokio::test]
async fn incompatible_initialize_and_modern_discovery_never_cross_on_upstream() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut discovery = BufReader::new(connect(&fixture.proxy).await?);
    let mut initialize = BufReader::new(connect(&fixture.proxy).await?);
    send(
        discovery.get_mut(),
        json!({"jsonrpc":"2.0","id":"discover","method":"server/discover","params":{}}),
    )
    .await?;
    let leader = fixture.request().await?;
    send(
        initialize.get_mut(),
        json!({"jsonrpc":"2.0","id":"initialize","method":"initialize","params":{}}),
    )
    .await?;
    let rejection = read(&mut initialize).await?;
    assert_eq!(rejection.get("id"), Some(&json!("initialize")));
    assert_eq!(rejection.pointer("/error/code"), Some(&json!(-32601)));
    assert!(fixture.requests.try_recv().is_err());

    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":leader.get("id"),"result":{
                "supportedVersions":["2026-07-28"],"capabilities":{}
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(
        read(&mut discovery).await?.get("id"),
        Some(&json!("discover"))
    );
    send(
        initialize.get_mut(),
        json!({"jsonrpc":"2.0","id":"initialize-again","method":"initialize","params":{}}),
    )
    .await?;
    let rejection = read(&mut initialize).await?;
    assert_eq!(rejection.get("id"), Some(&json!("initialize-again")));
    assert_eq!(rejection.pointer("/error/code"), Some(&json!(-32601)));
    assert!(fixture.requests.try_recv().is_err());
    fixture.stop().await
}

#[tokio::test]
async fn legacy_initialize_excludes_modern_discovery_in_flight_and_when_ready() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut initialize = BufReader::new(connect(&fixture.proxy).await?);
    let mut discovery = BufReader::new(connect(&fixture.proxy).await?);
    send(
        initialize.get_mut(),
        json!({"jsonrpc":"2.0","id":"initialize","method":"initialize","params":{}}),
    )
    .await?;
    let leader = fixture.request().await?;
    assert_eq!(leader.get("method"), Some(&json!("initialize")));
    for (id, ready) in [("discover-in-flight", false), ("discover-ready", true)] {
        if ready {
            fixture
                .responses
                .send(
                    json!({"jsonrpc":"2.0","id":leader.get("id"),"result":{
                        "protocolVersion":"2025-11-25","capabilities":{}
                    }})
                    .to_string(),
                )
                .await
                .map_err(io::Error::other)?;
            assert_eq!(
                read(&mut initialize).await?.get("id"),
                Some(&json!("initialize"))
            );
        }
        send(
            discovery.get_mut(),
            json!({"jsonrpc":"2.0","id":id,"method":"server/discover","params":{}}),
        )
        .await?;
        let rejection = read(&mut discovery).await?;
        assert_eq!(rejection.get("id"), Some(&json!(id)));
        assert_eq!(rejection.pointer("/error/code"), Some(&json!(-32601)));
        assert!(
            fixture.requests.try_recv().is_err(),
            "{id} reached upstream"
        );
    }
    fixture.stop().await
}

#[tokio::test]
async fn explicit_tools_list_ttl_controls_shared_cache_expiration() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut client = BufReader::new(connect(&fixture.proxy).await?);

    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"zero","method":"tools/list","params":{}}),
    )
    .await?;
    let zero = fixture.request().await?;
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":zero.get("id"),"result":{
                "tools":[],"ttlMs":0
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut client).await?.get("id"), Some(&json!("zero")));

    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"zero-again","method":"tools/list","params":{}}),
    )
    .await?;
    let zero_again = fixture.request().await?;
    assert_ne!(zero_again.get("id"), zero.get("id"));
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":zero_again.get("id"),"result":{
                "tools":[],"ttlMs":0
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(
        read(&mut client).await?.get("id"),
        Some(&json!("zero-again"))
    );

    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"private","method":"tools/list","params":{}}),
    )
    .await?;
    let private = fixture.request().await?;
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":private.get("id"),"result":{
                "tools":[],"ttlMs":1000,"cacheScope":"private"
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut client).await?.get("id"), Some(&json!("private")));
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"private-again","method":"tools/list","params":{}}),
    )
    .await?;
    let private_again = fixture.request().await?;
    assert_ne!(private_again.get("id"), private.get("id"));
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":private_again.get("id"),"result":{
                "tools":[],"ttlMs":0
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(
        read(&mut client).await?.get("id"),
        Some(&json!("private-again"))
    );

    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"short","method":"tools/list","params":{}}),
    )
    .await?;
    let short = fixture.request().await?;
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":short.get("id"),"result":{
                "tools":[],"ttlMs":20
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut client).await?.get("id"), Some(&json!("short")));
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"short-cached","method":"tools/list","params":{}}),
    )
    .await?;
    assert_eq!(
        read(&mut client).await?.get("id"),
        Some(&json!("short-cached"))
    );
    assert!(fixture.requests.try_recv().is_err());
    sleep(Duration::from_millis(25)).await;
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"short-expired","method":"tools/list","params":{}}),
    )
    .await?;
    let expired = fixture.request().await?;
    fixture
        .responses
        .send(json!({"jsonrpc":"2.0","id":expired.get("id"),"result":{"tools":[]}}).to_string())
        .await
        .map_err(io::Error::other)?;
    assert_eq!(
        read(&mut client).await?.get("id"),
        Some(&json!("short-expired"))
    );
    fixture.stop().await
}

#[tokio::test]
async fn tools_list_changed_during_in_flight_discovery_prevents_stale_cache() -> io::Result<()> {
    let mut fixture = Fixture::start().await?;
    let mut client = BufReader::new(connect(&fixture.proxy).await?);
    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"first","method":"tools/list","params":{}}),
    )
    .await?;
    let first = fixture.request().await?;

    fixture
        .responses
        .send(json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}).to_string())
        .await
        .map_err(io::Error::other)?;
    assert_eq!(
        read(&mut client).await?,
        json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"})
    );

    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":first.get("id"),"result":{
                "tools":[],"ttlMs":1000,"cacheScope":"public"
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut client).await?.get("id"), Some(&json!("first")));

    send(
        client.get_mut(),
        json!({"jsonrpc":"2.0","id":"second","method":"tools/list","params":{}}),
    )
    .await?;
    let second = fixture.request().await?;
    assert_ne!(second.get("id"), first.get("id"));
    fixture
        .responses
        .send(
            json!({"jsonrpc":"2.0","id":second.get("id"),"result":{
                "tools":[],"ttlMs":0
            }})
            .to_string(),
        )
        .await
        .map_err(io::Error::other)?;
    assert_eq!(read(&mut client).await?.get("id"), Some(&json!("second")));
    fixture.stop().await
}
