use std::io;
use std::time::Duration;

use tokio::process::{Child, Command};

#[cfg(unix)]
#[path = "upstream_process_unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "upstream_process_windows.rs"]
mod platform;

pub(crate) struct OwnedProcess {
    pub(crate) child: Child,
    ownership: platform::Ownership,
}

impl OwnedProcess {
    pub(crate) async fn spawn(mut command: Command) -> io::Result<Self> {
        let ownership = platform::Ownership::prepare(&mut command)?;
        let mut child = command.kill_on_drop(true).spawn()?;
        match ownership.attach(&child) {
            Ok(ownership) => Ok(Self { child, ownership }),
            Err(error) => {
                if let Err(kill_error) = child.start_kill() {
                    crate::diagnostics::log(format!(
                        "upstream_setup_kill_error error={kill_error}"
                    ));
                }
                match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
                    Ok(Ok(_)) => Err(error),
                    Ok(Err(wait_error)) => Err(io::Error::new(
                        io::ErrorKind::ResourceBusy,
                        format!("process setup failed: {error}; cleanup failed: {wait_error}"),
                    )),
                    Err(_) => Err(io::Error::new(
                        io::ErrorKind::ResourceBusy,
                        format!("process setup failed: {error}; cleanup was not confirmed"),
                    )),
                }
            }
        }
    }

    pub(crate) async fn wait_for_exit(&mut self) -> io::Result<()> {
        #[cfg(windows)]
        {
            self.child.wait().await.map(|_| ())
        }
        #[cfg(unix)]
        {
            self.ownership.wait_for_exit().await
        }
    }

    /// Terminates the owned process tree even if the immediate child has exited.
    pub(crate) async fn retire(&mut self) -> io::Result<()> {
        self.ownership.terminate()?;
        tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .map_err(|_| io::Error::other("upstream child retirement timed out"))??;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if self.ownership.is_empty()? {
                #[cfg(unix)]
                self.ownership.disarm();
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(io::Error::other(
                    "upstream process tree retirement was not confirmed",
                ));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
