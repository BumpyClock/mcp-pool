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
    // Holds the exclusive first pipe instance created eagerly in bind(). The
    // first accept() consumes it; interior mutability is required because
    // accept() takes &self. A parking_lot Mutex keeps this sync-only so the
    // guard is never held across an .await.
    #[cfg(windows)]
    first_instance:
        parking_lot::Mutex<Option<tokio::net::windows::named_pipe::NamedPipeServer>>,
}

/// Bind a listening endpoint at `path`.
/// - Unix: a Unix domain socket file. The parent dir is created, then bind is
///   attempted directly; an `AddrInUse` error is resolved by probing for a live
///   listener so a live owner wins (the caller exits) while only a genuinely
///   stale file is removed and rebound.
/// - Windows: eagerly creates the first pipe instance with
///   `first_pipe_instance(true)`, claiming the name exclusively so a second
///   process binding the same name fails. Later accepts create more instances.
pub fn bind(path: &Path) -> io::Result<LocalListener> {
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Singleton semantics: do NOT unconditionally unlink before bind. Two
        // cold-start daemons could otherwise both unlink and then bind separate
        // listeners on the same path (split brain). Instead bind first and only
        // on AddrInUse distinguish a live owner from a stale leftover file.
        match tokio::net::UnixListener::bind(path) {
            Ok(inner) => Ok(LocalListener { inner }),
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => {
                // A successful connect means a live daemon already owns this
                // path, so this losing daemon must surface AddrInUse and exit.
                if std::os::unix::net::UnixStream::connect(path).is_ok() {
                    return Err(error);
                }
                // No listener answered: the socket file is a stale leftover from
                // a daemon that crashed without cleaning up. Best-effort remove
                // (it may already be gone) then retry the bind exactly once.
                let _ = std::fs::remove_file(path);
                let inner = tokio::net::UnixListener::bind(path)?;
                Ok(LocalListener { inner })
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(windows)]
    {
        let pipe_name = path.to_string_lossy().to_string();
        // first_pipe_instance(true) gives Windows named pipes the singleton
        // semantics UnixListener::bind already provides: if another process
        // owns an instance of this name the OS rejects creation with
        // ERROR_ACCESS_DENIED, so the losing daemon's bind() errors and that
        // process exits, leaving exactly one pool owner.
        let first_instance = tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)?;
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
        let stream = tokio::net::UnixStream::connect(path).await?;
        Ok(Box::new(stream))
    }

    #[cfg(windows)]
    {
        let client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(path.to_string_lossy().as_ref())?;
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
            Ok(Box::new(stream))
        }

        #[cfg(windows)]
        {
            // Release the lock before connect().await so the sync guard never
            // spans a suspension point.
            let pre_created = self.first_instance.lock().take();
            let server = match pre_created {
                Some(server) => server,
                None => tokio::net::windows::named_pipe::ServerOptions::new()
                    .create(&self.pipe_name)?,
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
}
