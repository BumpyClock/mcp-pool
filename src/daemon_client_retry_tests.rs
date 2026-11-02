use super::*;
use anyhow::{Context, Result};
use std::future::ready;
use std::io;
use tokio::time::Instant;

#[tokio::test(start_paused = true)]
async fn startup_ready_endpoint_connects_without_artificial_delay() -> Result<()> {
    let started = Instant::now();
    let (client, _server) = tokio::io::duplex(16);
    let mut stream: Option<transport::LocalStream> = Some(Box::new(client));
    let mut attempts = 0;
    retry_connect_with(|| {
        attempts += 1;
        ready(
            stream
                .take()
                .ok_or_else(|| io::Error::other("extra connect")),
        )
    })
    .await?;
    assert_eq!(attempts, 1);
    assert_eq!(started.elapsed(), Duration::ZERO);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn startup_delayed_endpoint_is_retried_with_bounded_readiness_latency() -> Result<()> {
    let started = Instant::now();
    let ready_after = Duration::from_millis(35);
    let (client, _server) = tokio::io::duplex(16);
    let mut stream: Option<transport::LocalStream> = Some(Box::new(client));
    let mut attempts = 0;
    retry_connect_with(|| {
        attempts += 1;
        ready(if started.elapsed() < ready_after {
            Err(io::Error::from(io::ErrorKind::NotFound))
        } else {
            stream
                .take()
                .ok_or_else(|| io::Error::other("extra connect"))
        })
    })
    .await?;
    assert!(attempts > 1);
    assert!(started.elapsed() >= ready_after);
    assert!(started.elapsed() <= ready_after + Duration::from_millis(100));
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn startup_permission_failure_does_not_wait_or_retry() -> Result<()> {
    let started = Instant::now();
    let mut attempts = 0;
    let error = retry_connect_with(|| {
        attempts += 1;
        ready(Err(io::Error::from(io::ErrorKind::PermissionDenied)))
    })
    .await
    .err()
    .context("expected permission error")?;
    assert_eq!(attempts, 1);
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(
        error.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::PermissionDenied)
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn startup_unavailable_endpoint_stops_at_original_three_second_budget() -> Result<()> {
    let started = Instant::now();
    let mut attempts = 0;
    let error = retry_connect_with(|| {
        assert!(started.elapsed() < Duration::from_secs(3));
        attempts += 1;
        ready(Err(io::Error::from(io::ErrorKind::ConnectionRefused)))
    })
    .await
    .err()
    .context("expected startup deadline")?;
    assert!(attempts > 1);
    assert_eq!(started.elapsed(), Duration::from_secs(3));
    assert!(
        error
            .to_string()
            .contains("could not reach daemon after launch")
    );
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn startup_deadline_also_bounds_a_stalled_connector() -> Result<()> {
    for initially_absent in [false, true] {
        let started = Instant::now();
        let mut attempts = 0;
        let error = retry_connect_with(|| {
            attempts += 1;
            let absent = initially_absent && attempts == 1;
            async move {
                if absent {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                } else {
                    std::future::pending().await
                }
            }
        })
        .await
        .err()
        .context("expected stalled connection deadline")?;
        assert_eq!(attempts, if initially_absent { 2 } else { 1 });
        assert_eq!(started.elapsed(), Duration::from_secs(3));
        assert!(
            error
                .to_string()
                .contains("could not reach daemon after launch")
        );
    }
    Ok(())
}
