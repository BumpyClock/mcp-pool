use anyhow::Result;
use serde_json::json;

use super::test_support::Fixture;
use super::{context, prepare};

#[tokio::test(flavor = "current_thread")]
async fn stdio_preparation_completes_without_yielding_or_reading_credentials() -> Result<()> {
    let mut fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.server.definition.url.clear();
    fixture.server.definition.command = "synthetic-command".to_owned();
    fixture.server.definition.auth = Some(context(&fixture.server)?);
    let path = context(&fixture.server)?.vault_path;
    std::fs::create_dir_all(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("fixture vault has no parent"))?,
    )?;
    std::fs::write(&path, b"not a credential store")?;
    let mut preparation = std::pin::pin!(prepare(&fixture.server, true));
    let definition = std::future::poll_fn(|task_context| {
        std::task::Poll::Ready(preparation.as_mut().poll(task_context))
    })
    .await;
    let std::task::Poll::Ready(definition) = definition else {
        anyhow::bail!("stdio credential preparation must not yield");
    };
    let definition = definition?;
    assert!(definition.auth.is_none());
    assert_eq!(definition.command, "synthetic-command");
    assert_eq!(std::fs::read(path)?, b"not a credential store");
    Ok(())
}

#[tokio::test]
async fn remote_preparation_without_oauth_does_not_create_credentials() -> Result<()> {
    let mut fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    fixture.server.raw = json!({});
    let path = context(&fixture.server)?.vault_path;
    assert!(prepare(&fixture.server, false).await?.auth.is_none());
    assert!(!path.exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn remote_credential_preflight_yields_while_store_read_blocks() -> Result<()> {
    use std::ffi::CString;
    use std::io::Write;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};

    let mut fixture = Fixture::new("https://synthetic.invalid/mcp")?;
    // No tokens or explicit OAuth means only the identity inspection reads the FIFO.
    fixture.server.raw = json!({});
    let path = context(&fixture.server)?.vault_path;
    std::fs::create_dir_all(
        path.parent()
            .ok_or_else(|| anyhow::anyhow!("fixture vault has no parent"))?,
    )?;
    let filename = CString::new(path.as_os_str().as_bytes())?;
    if unsafe { libc::mkfifo(filename.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let payload = br#"{"version":2,"entries":{},"serverUrls":{}}"#;
    let replacement = path.with_extension("replacement");
    std::fs::write(&replacement, payload)?;
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || -> Result<bool> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let (mut file, paired) = loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&path)
            {
                Ok(file) => break (file, true),
                Err(error) if error.raw_os_error() == Some(libc::ENXIO) => {
                    if Instant::now() >= deadline {
                        // Wake a late reader too, rather than leaving a blocking job behind.
                        let file = std::fs::OpenOptions::new()
                            .read(true)
                            .write(true)
                            .custom_flags(libc::O_NONBLOCK)
                            .open(&path)?;
                        break (file, false);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        };
        // Future reads see a regular store, even if preparation unexpectedly rereads it.
        let replaced = std::fs::rename(replacement, &path);
        let signaled = entered.send(()).is_ok();
        let yielded = paired && signaled && released.recv_timeout(Duration::from_secs(5)).is_ok();
        // Always release the FIFO before propagating controller or replacement errors.
        let written = file.write_all(payload);
        drop(file);
        replaced?;
        written?;
        Ok(yielded)
    });
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        tokio::join!(prepare(&fixture.server, false), async move {
            observed.await?;
            release.send(())?;
            Ok::<(), anyhow::Error>(())
        })
    })
    .await;
    // Join even on timeout or preparation failure before Fixture removes the store.
    let yielded = writer
        .join()
        .map_err(|_| anyhow::anyhow!("synthetic credential writer panicked"))??;
    let (prepared, probe) = result?;
    probe?;
    assert!(prepared?.auth.is_none());
    assert!(
        yielded,
        "credential I/O must not block unrelated runtime work"
    );
    Ok(())
}
