use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::{daemon_client, transport};

pub(super) async fn wait(path: &Path) -> Result<()> {
    wait_with(|| transport::connect(path), Duration::from_secs(15)).await
}

async fn wait_with<Connect, Connection>(mut connect: Connect, budget: Duration) -> Result<()>
where
    Connect: FnMut() -> Connection,
    Connection: std::future::Future<Output = std::io::Result<transport::LocalStream>>,
{
    tokio::time::timeout(budget, async {
        loop {
            match connect().await {
                Ok(stream) => drop(stream),
                Err(error) if daemon_client::endpoint_absent(&error) => return Ok(()),
                Err(error) if daemon_client::endpoint_busy(&error) => {}
                // Linux can reset an in-progress Unix connect as the listener closes.
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                Err(error) => return Err(error).context("Verifying daemon endpoint retirement"),
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("Daemon retirement not verified within 15 seconds; replacement was not started")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::future::ready;
    use std::io;

    #[tokio::test(start_paused = true)]
    async fn closing_connections_require_a_later_absence_observation() -> Result<()> {
        let (stream, peer) = tokio::io::duplex(16);
        let mut observations: VecDeque<io::Result<transport::LocalStream>> = VecDeque::from([
            Err(io::Error::from(io::ErrorKind::ConnectionReset)),
            Ok(Box::new(stream) as transport::LocalStream),
            Err(io::Error::from(io::ErrorKind::WouldBlock)),
            Err(io::Error::from(io::ErrorKind::ConnectionRefused)),
        ]);
        wait_with(
            || {
                ready(
                    observations
                        .pop_front()
                        .unwrap_or_else(|| Err(io::Error::other("extra probe"))),
                )
            },
            Duration::from_secs(1),
        )
        .await?;
        assert!(observations.is_empty());
        drop(peer);
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn resets_never_confirm_retirement_and_security_errors_do_not_retry() -> Result<()> {
        let mut probes = 0;
        let denied = wait_with(
            || {
                probes += 1;
                ready(Err(io::Error::from(io::ErrorKind::PermissionDenied)))
            },
            Duration::from_secs(1),
        )
        .await;
        assert!(denied.is_err());
        assert_eq!(probes, 1);
        let unsettled = wait_with(
            || ready(Err(io::Error::from(io::ErrorKind::ConnectionReset))),
            Duration::from_millis(100),
        )
        .await;
        assert!(unsettled.is_err());
        Ok(())
    }
}
