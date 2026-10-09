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
    // Holds the secured exclusive first pipe instance created eagerly in bind().
    // Later accepts create instances with the same DACL and remote-client rule.
    #[cfg(windows)]
    first_instance: parking_lot::Mutex<Option<tokio::net::windows::named_pipe::NamedPipeServer>>,
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
            first_instance: parking_lot::Mutex::new(Some(first_instance)),
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
            // Release the lock before connect().await so the sync guard never
            // spans a suspension point.
            let pre_created = self.first_instance.lock().take();
            let server = match pre_created {
                Some(server) => server,
                None => crate::local_security::create_named_pipe(&self.pipe_name, false)?,
            };
            server.connect().await?;
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
}
