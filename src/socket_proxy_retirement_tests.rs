use super::lifecycle_tests::{Backend, backend, proxy};
use super::*;
use crate::pool::Pool;

pub(crate) const RETIREMENT_ERROR: &str = "tree retirement unverified";

pub(crate) async fn failed_retirement_pool() -> io::Result<Arc<Pool>> {
    let proxy = proxy();
    let Backend {
        setup,
        handle,
        retired,
        responses,
        ..
    } = backend(&proxy);
    setup
        .send(Ok(handle))
        .map_err(|_| io::Error::other("setup lost"))?;
    proxy.start().await?;
    let generation = proxy
        .generation
        .lock()
        .clone()
        .ok_or_else(|| io::Error::other("missing generation"))?;
    retired.send_replace(Some(Err(RETIREMENT_ERROR.to_string())));
    let error = tokio::time::timeout(
        Duration::from_secs(2),
        generation::wait_completion(generation.completion.clone()),
    )
    .await
    .map_err(io::Error::other)?;
    assert_eq!(
        error.err().map(|error| error.to_string()),
        Some(RETIREMENT_ERROR.to_string())
    );
    let pool = Arc::new(Pool::new());
    pool.insert_test_proxy("failed-server", proxy);
    drop(responses);
    Ok(pool)
}

fn retained_failure(pool: &Pool) {
    let status = pool.get_status();
    assert_eq!(status.server_count, 1);
    let server = status.servers.first();
    assert_eq!(
        server.map(|server| server.status),
        Some(ServerStatus::Failed)
    );
    assert_eq!(
        server.and_then(|server| server.readiness.retirement_error.as_deref()),
        Some(RETIREMENT_ERROR)
    );
}

#[tokio::test]
async fn failed_retirement_propagates_and_registry_entry_blocks_replacement() -> io::Result<()> {
    let pool = failed_retirement_pool().await?;
    assert_eq!(
        pool.stop_server("failed-server")
            .await
            .err()
            .map(|error| error.to_string()),
        Some(RETIREMENT_ERROR.to_string())
    );
    retained_failure(&pool);
    assert_eq!(
        pool.restart("failed-server")
            .await
            .err()
            .map(|error| error.to_string()),
        Some(RETIREMENT_ERROR.to_string())
    );
    retained_failure(&pool);
    let replacement = UpstreamSpec::Stdio {
        command: "replacement-must-not-launch".to_string(),
        args: Vec::new(),
        env: Default::default(),
    };
    assert_eq!(
        pool.start("failed-server", replacement)
            .await
            .err()
            .map(|error| error.to_string()),
        Some(RETIREMENT_ERROR.to_string())
    );
    retained_failure(&pool);
    assert_eq!(
        pool.shutdown().await.err().map(|error| error.to_string()),
        Some(RETIREMENT_ERROR.to_string())
    );
    retained_failure(&pool);
    Ok(())
}
