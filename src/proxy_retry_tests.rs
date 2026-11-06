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

#[tokio::test]
async fn remote_eof_finishes_while_stdin_remains_open() -> std::io::Result<()> {
    let (_input_writer, input) = tokio::io::duplex(64);
    let (local, remote) = tokio::io::duplex(64);
    let (reader, writer) = tokio::io::split(local);
    drop(remote);
    tokio::time::timeout(
        Duration::from_secs(1),
        bridge_transfers(input, tokio::io::sink(), reader, writer, "remote-eof"),
    )
    .await
    .map_err(std::io::Error::other)?
}

#[tokio::test]
async fn broken_output_is_returned_while_stdin_remains_open() -> std::io::Result<()> {
    let (_input_writer, input) = tokio::io::duplex(64);
    let (local, mut remote) = tokio::io::duplex(64);
    let (reader, writer) = tokio::io::split(local);
    let (output, closed_reader) = tokio::io::duplex(64);
    drop(closed_reader);
    remote.write_all(b"response").await?;
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        bridge_transfers(input, output, reader, writer, "broken-output"),
    )
    .await
    .map_err(std::io::Error::other)?;
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(std::io::ErrorKind::BrokenPipe)
    );
    Ok(())
}

#[tokio::test]
async fn stdin_eof_drains_a_final_response_without_closing_the_socket() -> std::io::Result<()> {
    let (local, mut remote) = tokio::io::duplex(64);
    let (reader, writer) = tokio::io::split(local);
    let mut output = Vec::new();
    let bridge = bridge_transfers(tokio::io::empty(), &mut output, reader, writer, "drain");
    let upstream = async {
        use std::future::{Future, poll_fn};
        use std::task::Poll;
        let mut byte = [0];
        {
            let reading = remote.read(&mut byte);
            tokio::pin!(reading);
            poll_fn(|context| {
                assert!(reading.as_mut().poll(context).is_pending());
                Poll::Ready(())
            })
            .await;
        }
        remote.write_all(b"final response").await?;
        remote.shutdown().await
    };
    tokio::time::timeout(Duration::from_secs(1), async {
        tokio::try_join!(bridge, upstream)
    })
    .await
    .map_err(std::io::Error::other)??;
    assert_eq!(output, b"final response");
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn response_drain_expires_after_800ms_and_drops_the_socket() -> std::io::Result<()> {
    let (local, mut remote) = tokio::io::duplex(64);
    let (reader, writer) = tokio::io::split(local);
    let started = tokio::time::Instant::now();
    bridge_transfers(
        tokio::io::empty(),
        tokio::io::sink(),
        reader,
        writer,
        "drain-expiry",
    )
    .await?;
    assert_eq!(started.elapsed(), Duration::from_millis(800));
    assert_eq!(
        remote
            .write_all(b"late response")
            .await
            .err()
            .map(|error| error.kind()),
        Some(std::io::ErrorKind::BrokenPipe)
    );
    Ok(())
}

struct FailedReader;

impl AsyncRead for FailedReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
        _buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::other("fixture read failure")))
    }
}

#[tokio::test]
async fn input_read_failure_is_returned_without_waiting_for_drain() -> std::io::Result<()> {
    let (local, _remote) = tokio::io::duplex(64);
    let (reader, writer) = tokio::io::split(local);
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        bridge_transfers(
            FailedReader,
            tokio::io::sink(),
            reader,
            writer,
            "broken-input",
        ),
    )
    .await
    .map_err(std::io::Error::other)?;
    assert_eq!(
        result.err().map(|error| error.to_string()).as_deref(),
        Some("fixture read failure")
    );
    Ok(())
}
