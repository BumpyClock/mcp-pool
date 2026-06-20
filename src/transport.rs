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
}

/// Bind a listening endpoint at `path`.
/// - Unix: a Unix domain socket file (parent dir created, stale file removed).
/// - Windows: records the pipe name; a fresh pipe instance is created per accept.
pub fn bind(path: &Path) -> io::Result<LocalListener> {
    #[cfg(unix)]
    {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        let inner = tokio::net::UnixListener::bind(path)?;
        Ok(LocalListener { inner })
    }

    #[cfg(windows)]
    {
        Ok(LocalListener {
            pipe_name: path.to_string_lossy().to_string(),
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
    pub async fn accept(&self) -> io::Result<LocalStream> {
        #[cfg(unix)]
        {
            let (stream, _) = self.inner.accept().await?;
            Ok(Box::new(stream))
        }

        #[cfg(windows)]
        {
            let server = tokio::net::windows::named_pipe::ServerOptions::new()
                .create(&self.pipe_name)?;
            server.connect().await?;
            Ok(Box::new(server))
        }
    }
}
