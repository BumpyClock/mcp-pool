use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;

use super::{Fixture, ToolFilter, parse, test_fixture, tool};

#[test]
fn concurrency_accepts_only_bounded_positive_integers() -> Result<()> {
    for value in ["1", "8", "64"] {
        let expected = value.parse::<usize>()?;
        assert_eq!(
            parse(vec![format!("--concurrency={value}")])?.concurrency,
            expected
        );
        assert_eq!(
            parse(vec!["--concurrency".into(), value.into()])?.concurrency,
            expected
        );
    }
    for arguments in [
        vec!["--concurrency"],
        vec!["--concurrency="],
        vec!["--concurrency=0"],
        vec!["--concurrency=65"],
        vec!["--concurrency=-1"],
        vec!["--concurrency=1.5"],
    ] {
        assert!(parse(arguments.into_iter().map(str::to_owned).collect()).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn opt_in_concurrency_allows_fast_calls_and_discovery_past_a_held_call() -> Result<()> {
    let fixture = Fixture::new("alpha", vec![tool("echo", "echo")]);
    let state = Arc::new(
        test_fixture::concurrent_state(
            vec![(
                fixture.clone(),
                ToolFilter::default(),
                Duration::from_secs(5),
            )],
            2,
        )
        .await?,
    );
    let held_state = Arc::clone(&state);
    let held = tokio::spawn(async move {
        held_state
            .call_tool("alpha__echo", json!({"label":"hold"}), None)
            .await
    });
    fixture.wait_for(1, 0, 1).await?;
    let fast = state
        .call_tool("alpha__echo", json!({"label":"fast"}), None)
        .await
        .map_err(|_| anyhow::anyhow!("fast call blocked behind held call"))?;
    assert_eq!(fast.pointer("/echo/arguments/label"), Some(&json!("fast")));
    let tools = state
        .list_tools(None)
        .await
        .map_err(|_| anyhow::anyhow!("discovery blocked"))?;
    assert_eq!(tools.len(), 1);
    assert!(!held.is_finished());
    fixture.release_call();
    assert!(held.await?.is_ok());
    assert_eq!(fixture.connections(), 2);
    assert_eq!(fixture.calls().await.len(), 2);
    Ok(())
}

#[tokio::test]
async fn full_pool_bounds_connections_and_queue_wait_without_dispatch() -> Result<()> {
    let fixture = Fixture::new("alpha", vec![tool("echo", "echo")]);
    let mut state = test_fixture::concurrent_state(
        vec![(
            fixture.clone(),
            ToolFilter::default(),
            Duration::from_secs(5),
        )],
        2,
    )
    .await?;
    let server = state.clients.get_mut("alpha").context("missing server")?;
    server.timeout = Duration::from_millis(100);
    let state = Arc::new(state);
    // Hold leases directly so queued operations cannot consume their own request budgets.
    let mut held = Vec::new();
    for _ in 0..2 {
        let server = state.clients.get("alpha").context("missing server")?;
        let client = Arc::clone(&server.client);
        let reconnect = Arc::clone(&server.reconnect);
        held.push(tokio::spawn(async move {
            client
                .with_client(&reconnect, |client| {
                    Box::pin(async move {
                        client
                            .request(
                                "tools/call",
                                json!({"name":"echo", "arguments":{"label":"hold"}}),
                            )
                            .await
                    })
                })
                .await
        }));
    }

    fixture.wait_for(2, 0, 2).await?;
    let queued = state
        .call_tool("alpha__echo", json!({"label":"not-sent"}), None)
        .await;
    assert!(queued.is_err());
    assert_eq!(fixture.connections(), 2);
    assert_eq!(fixture.calls().await.len(), 2);
    for task in held {
        task.abort();
        assert!(task.await.is_err());
    }
    fixture.wait_for(2, 2, 2).await?;
    let fast = state
        .call_tool("alpha__echo", json!({"label":"after"}), None)
        .await
        .map_err(|_| anyhow::anyhow!("cancelled slot not reusable"))?;
    assert_eq!(fast.pointer("/echo/arguments/label"), Some(&json!("after")));
    assert_eq!(fixture.calls().await.len(), 3);
    Ok(())
}

#[tokio::test]
async fn many_calls_reuse_only_the_configured_number_of_connections() -> Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let fixture = Fixture::new("alpha", vec![tool("echo", "echo")])
        .with_activity(Arc::clone(&active), Arc::clone(&maximum));
    let state = Arc::new(
        test_fixture::concurrent_state(
            vec![(
                fixture.clone(),
                ToolFilter::default(),
                Duration::from_secs(5),
            )],
            2,
        )
        .await?,
    );
    let mut calls = tokio::task::JoinSet::new();
    for label in 0..8 {
        let state = Arc::clone(&state);
        calls.spawn(async move {
            let result = state
                .call_tool("alpha__echo", json!({"label":label}), None)
                .await
                .map_err(|_| anyhow::anyhow!("concurrent call failed"))?;
            assert_eq!(result.pointer("/echo/arguments/label"), Some(&json!(label)));
            Ok::<_, anyhow::Error>(())
        });
    }
    while let Some(call) = calls.join_next().await {
        call??;
    }
    assert_eq!(fixture.connections(), 2);
    assert_eq!(fixture.calls().await.len(), 8);
    assert_eq!(maximum.load(Ordering::SeqCst), 2);
    assert_eq!(active.load(Ordering::SeqCst), 0);
    Ok(())
}
