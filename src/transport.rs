use std::io;
use std::path::Path;

use tokio::io::{AsyncRead, AsyncWrite};

/// Combined read+write trait so a single `dyn` object can stand in for both
/// Unix sockets and Windows named pipes.
pub trait LocalIo: AsyncRead + AsyncWrite {}
impl<T> LocalIo for T where T: AsyncRead + AsyncWrite {}

/// Unified bidirectional local stream. Boxed trait object so Unix domain sockets
/// and Windows named pipes share one type across the multiplexer and proxy code.
pub type LocalStream = Box<dyn LocalIo + Unpin + Send>;

pub struct LocalListener {
    #[cfg(unix)]
    inner: tokio::net::UnixListener,
    #[cfg(windows)]
    pipe_name: String,
    // Keep pending instances in the listener because select! may cancel accept.
    #[cfg(windows)]
    pending_instance: tokio::sync::Mutex<Option<tokio::net::windows::named_pipe::NamedPipeServer>>,
}

/// Bind a listening endpoint at `path`.
/// - Unix: private application directories, a mode-0600 socket, and same-user
///   peer checks; only a validated same-user stale socket can be removed.
/// - Windows: eagerly creates the first pipe instance with
///   `first_pipe_instance(true)`, an owner-only DACL, and remote-client
///   rejection. Later accepts create instances with the same protections.
pub fn bind(path: &Path) -> io::Result<LocalListener> {
    #[cfg(unix)]
    {
        let inner = crate::local_security::bind_unix_listener(path)?;
        Ok(LocalListener { inner })
    }

    #[cfg(windows)]
    {
        let pipe_name = path.to_string_lossy().to_string();
        // The first pipe instance claims the name exclusively so a second
        // daemon cannot start even when it supplies the same DACL.
        let first_instance = crate::local_security::create_named_pipe(&pipe_name, true)?;
        Ok(LocalListener {
            pipe_name,
            pending_instance: tokio::sync::Mutex::new(Some(first_instance)),
        })
    }
}

/// Connect to a bound endpoint as a client.
pub async fn connect(path: &Path) -> io::Result<LocalStream> {
    #[cfg(unix)]
    {
        let endpoint = crate::local_security::validate_unix_socket_path(path)?;
        let stream = tokio::net::UnixStream::connect(endpoint).await?;
        crate::local_security::verify_unix_peer(&stream)?;
        Ok(Box::new(stream))
    }

    #[cfg(windows)]
    {
        let client = crate::local_security::connect_named_pipe(path)?;
        Ok(Box::new(client))
    }
}

impl LocalListener {
    /// Accept one client connection and return a unified stream.
    ///
    /// On Windows the first accept reuses the exclusive instance created in
    /// bind(); every later accept creates a fresh instance, which must NOT set
    /// first_pipe_instance (only the first instance of a name may claim it).
    pub async fn accept(&self) -> io::Result<LocalStream> {
        #[cfg(unix)]
        {
            let (stream, _) = self.inner.accept().await?;
            crate::local_security::verify_unix_peer(&stream)?;
            Ok(Box::new(stream))
        }

        #[cfg(windows)]
        {
            let mut pending = self.pending_instance.lock().await;
            if pending.is_none() {
                *pending = Some(crate::local_security::create_named_pipe(&self.pipe_name, false)?);
            }
            let server = pending.as_ref().ok_or_else(|| io::Error::other("missing pending pipe"))?;
            server.connect().await?;
            let server = pending.take().ok_or_else(|| io::Error::other("missing connected pipe"))?;
            Ok(Box::new(server))
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    fn unique_endpoint() -> std::path::PathBuf {
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let name = format!("test-{}-{n}", std::process::id());
        // Reuse the production path helper so the endpoint matches the real
        // socket/pipe scheme on every platform (and avoids a backslash literal).
        crate::config::server_socket_path(&name)
    }

    #[tokio::test]
    async fn bind_accept_connect_round_trip() {
        let path = unique_endpoint();
        let listener = bind(&path).expect("bind failed");
        let server = tokio::spawn(async move {
            listener.accept().await.expect("accept failed");
        });
        // On Windows the listener creates its pipe instance lazily inside
        // accept(), so the client may need to retry until it rendezvouses.
        let mut connected = false;
        for _ in 0..50 {
            if connect(&path).await.is_ok() {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(connected, "client could not connect to bound endpoint");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn a_live_listener_keeps_singleton_bind_ownership() -> io::Result<()> {
        let path = unique_endpoint();
        let listener = bind(&path)?;
        let duplicate = bind(&path);
        #[cfg(unix)]
        assert_eq!(
            duplicate.err().map(|error| error.kind()),
            Some(io::ErrorKind::AddrInUse)
        );
        #[cfg(windows)]
        assert_eq!(
            duplicate.err().map(|error| error.kind()),
            Some(io::ErrorKind::PermissionDenied)
        );
        drop(listener);
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_accept_preserves_the_pending_endpoint() -> io::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let path = unique_endpoint();
        let listener = bind(&path)?;
        assert!(tokio::time::timeout(Duration::from_millis(20), listener.accept()).await.is_err());
        let mut client = connect(&path).await?;
        let mut server = listener.accept().await?;
        client.write_all(b"retained").await?;
        let mut received = [0; 8];
        server.read_exact(&mut received).await?;
        assert_eq!(&received, b"retained");
        drop(server);
        drop(client);
        drop(listener);
        Ok(())
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn abandoned_client_does_not_poison_later_accepts() -> io::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let path = unique_endpoint();
        let listener = bind(&path)?;
        let abandoned = connect(&path).await?;
        drop(abandoned);

        let mut abandoned_server = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
        let mut buffer = [0; 1];
        let received =
            tokio::time::timeout(Duration::from_secs(5), abandoned_server.read(&mut buffer))
                .await
                .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
        assert_eq!(received, 0);
        assert!(tokio::time::timeout(Duration::from_millis(20), listener.accept()).await.is_err());
        drop(abandoned_server);
        assert_eq!(
            bind(&path).err().map(|error| error.kind()),
            Some(io::ErrorKind::PermissionDenied)
        );

        let mut client = connect(&path).await?;
        let mut server = tokio::time::timeout(Duration::from_secs(5), listener.accept())
            .await
            .map_err(|error| io::Error::new(io::ErrorKind::TimedOut, error))??;
        client.write_all(b"recovered").await?;
        let mut received = [0; 9];
        server.read_exact(&mut received).await?;
        assert_eq!(&received, b"recovered");
        Ok(())
    }
}
