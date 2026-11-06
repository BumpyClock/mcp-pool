use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{FixtureLifecycle, fixture_home};

fn executable() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/common/fixture_control.sh")
}

fn controlled() -> io::Result<(PathBuf, FixtureLifecycle)> {
    let home = fixture_home::create("fixture-fault")?;
    let executable = executable();
    let lifecycle = FixtureLifecycle::new(&executable, &home, &home, &home, None);
    Ok((home, lifecycle))
}

#[tokio::test]
async fn failed_and_hanging_retirement_preserve_namespace_until_retry() -> io::Result<()> {
    for fault in ["fail", "hang", "stay-alive"] {
        let (home, mut lifecycle) = controlled()?;
        let mut start = lifecycle.command();
        start.args(["pool", "serve"]);
        lifecycle.start(start)?;
        lifecycle.ready(Duration::from_secs(2)).await?;
        std::fs::write(home.join("evidence"), b"retain until retired")?;
        std::fs::write(home.join(fault), b"")?;
        let began = Instant::now();
        let error = lifecycle
            .retire(Duration::from_millis(200))
            .err()
            .ok_or_else(|| io::Error::other("unsafe retirement succeeded"))?;
        assert!(began.elapsed() < Duration::from_secs(3), "{error}");
        assert_eq!(
            std::fs::read(home.join("evidence"))?,
            b"retain until retired"
        );
        std::fs::remove_file(home.join(fault))?;
        lifecycle.finish().await?;
        assert!(!home.exists());
    }
    Ok(())
}

#[test]
fn drop_retains_failed_and_hanging_shutdown_for_later_retry() -> io::Result<()> {
    for fault in ["fail", "hang"] {
        let (home, lifecycle) = controlled()?;
        std::fs::write(home.join(fault), b"")?;
        std::fs::write(home.join("evidence"), b"retain")?;
        let began = Instant::now();
        drop(lifecycle);
        assert!(began.elapsed() < Duration::from_secs(30));
        assert_eq!(std::fs::read(home.join("evidence"))?, b"retain");
        std::fs::remove_file(home.join(fault))?;
        let mut retry = FixtureLifecycle::new(&executable(), &home, &home, &home, None);
        retry.retire(Duration::from_secs(2))?;
        assert!(!home.exists());
    }
    let (home, lifecycle) = controlled()?;
    drop(lifecycle);
    assert!(!home.exists());
    Ok(())
}

#[tokio::test]
async fn readiness_retries_unsuccessful_startup_observations() -> io::Result<()> {
    let (home, mut lifecycle) = controlled()?;
    std::fs::write(home.join("startup-not-ready"), b"")?;
    let mut start = lifecycle.command();
    start.args(["pool", "serve"]);
    lifecycle.start(start)?;
    lifecycle.ready(Duration::from_secs(2)).await?;
    assert!(!home.join("startup-not-ready").exists());
    lifecycle.finish().await?;
    assert!(!home.exists());
    Ok(())
}

#[tokio::test]
async fn readiness_rejects_early_exit_and_malformed_status() -> io::Result<()> {
    for fault in ["exit-early", "malformed", "status-hang"] {
        let (home, mut lifecycle) = controlled()?;
        std::fs::write(home.join(fault), b"")?;
        let mut start = lifecycle.command();
        start.args(["pool", "serve"]);
        lifecycle.start(start)?;
        let began = Instant::now();
        let error = lifecycle
            .ready(Duration::from_millis(300))
            .await
            .err()
            .ok_or_else(|| io::Error::other("invalid readiness accepted"))?;
        assert!(began.elapsed() < Duration::from_secs(2));
        match fault {
            "exit-early" => assert!(
                error.to_string().contains("exited before readiness"),
                "{error}"
            ),
            "malformed" => assert!(
                error.to_string().contains("malformed daemon status"),
                "{error}"
            ),
            _ => assert_eq!(error.kind(), io::ErrorKind::TimedOut),
        }
        std::fs::remove_file(home.join(fault))?;
        lifecycle.finish().await?;
        assert!(!home.exists());
    }
    Ok(())
}

#[test]
fn shutdown_acknowledgement_requires_stopped_status_before_removal() -> io::Result<()> {
    let (home, mut lifecycle) = controlled()?;
    std::fs::write(home.join("stay-alive"), b"")?;
    let error = lifecycle
        .retire(Duration::from_secs(2))
        .err()
        .ok_or_else(|| io::Error::other("live endpoint was removed"))?;
    assert!(error.to_string().contains("still running"), "{error}");
    assert!(home.exists());
    std::fs::remove_file(home.join("stay-alive"))?;
    lifecycle.retire(Duration::from_secs(2))?;
    assert!(!home.exists());
    Ok(())
}
