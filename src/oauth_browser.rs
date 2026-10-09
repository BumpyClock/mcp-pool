use std::time::Duration;

use anyhow::{Result, bail};
use reqwest::Url;

pub(super) async fn open(url: &Url) -> Result<()> {
    #[cfg(windows)]
    let mut command = {
        let mut command = tokio::process::Command::new("rundll32");
        command.arg("url.dll,FileProtocolHandler");
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = tokio::process::Command::new("open");
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = tokio::process::Command::new("xdg-open");
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        command
            .arg(url.as_str())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("browser launch timed out; rerun auth --no-browser"))?
    .map_err(|_| anyhow::anyhow!("browser could not open; rerun auth --no-browser"))?;
    if !status.success() {
        bail!("browser launch failed; rerun auth --no-browser");
    }
    Ok(())
}
