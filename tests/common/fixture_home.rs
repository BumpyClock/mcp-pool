use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// Exclusively creates a fixture home whose path leaves room for pooled Unix socket names.
pub fn create(prefix: &str) -> io::Result<PathBuf> {
    // macOS TMPDIR paths leave too few sun_path bytes for pooled server sockets.
    #[cfg(unix)]
    let root = PathBuf::from("/tmp");
    #[cfg(windows)]
    let root = std::env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(io::Error::other)?
        .subsec_nanos();
    let home = root.join(format!(
        "{prefix}-{}-{}-{nonce:x}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::SeqCst)
    ));
    // Exclusive creation refuses a pre-existing directory or symlink in the shared root.
    std::fs::create_dir(&home)?;
    Ok(home)
}
