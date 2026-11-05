use super::*;

#[tokio::test(start_paused = true)]
async fn unavailable_endpoint_retries_and_attaches_when_listener_becomes_ready()
-> std::io::Result<()> {
    use std::future::{Future, poll_fn};
    use std::task::Poll;

    #[cfg(unix)]
    let (path, directory) = {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("mcp-cli-proxy-ready-{}", std::process::id()));
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        (directory.join("ready.sock"), directory)
    };
    #[cfg(windows)]
    let path = std::path::PathBuf::from(format!(
        r"\\.\pipe\mcp-cli-proxy-ready-{}",
        std::process::id()
    ));
    let start = tokio::time::Instant::now();
    let connection = connect_with_retry(&path);
    tokio::pin!(connection);
    poll_fn(|context| {
        assert!(connection.as_mut().poll(context).is_pending());
        Poll::Ready(())
    })
    .await;
    let listener = transport::bind(&path)?;
    tokio::time::advance(Duration::from_millis(100)).await;
    let (client, server) = tokio::try_join!(&mut connection, listener.accept())?;
    assert_eq!(start.elapsed(), Duration::from_millis(100));
    drop(client);
    drop(server);
    drop(listener);
    #[cfg(unix)]
    {
        std::fs::remove_file(&path)?;
        std::fs::remove_dir(directory)?;
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn permanent_local_connect_error_has_no_retry_delay() -> std::io::Result<()> {
    #[cfg(unix)]
    let (path, directory) = {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("mcp-cli-proxy-retry-{}", std::process::id()));
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let path = directory.join("not-a-socket");
        std::fs::write(&path, b"fixture")?;
        (path, directory)
    };
    #[cfg(windows)]
    let path = std::path::PathBuf::from(r"\\remote\pipe\invalid");
    let start = tokio::time::Instant::now();
    let error = match connect_with_retry(&path).await {
        Ok(_) => return Err(std::io::Error::other("invalid endpoint connected")),
        Err(error) => error,
    };
    #[cfg(unix)]
    {
        std::fs::remove_file(&path)?;
        std::fs::remove_dir(directory)?;
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }
    #[cfg(windows)]
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(start.elapsed(), Duration::ZERO);
    Ok(())
}
