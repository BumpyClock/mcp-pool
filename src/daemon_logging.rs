use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result, bail};

struct Sink {
    file: std::fs::File,
    servers: BTreeSet<String>,
    identities: BTreeMap<String, String>,
}

static SINK: OnceLock<Mutex<Option<Sink>>> = OnceLock::new();
const MAX_BYTES: u64 = 5 * 1024 * 1024;

pub(crate) fn configure(path: PathBuf, servers: BTreeSet<String>) -> Result<()> {
    if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("Daemon log must not be a symbolic link");
    }
    if let Some(directory) = path
        .parent()
        .filter(|directory| !directory.as_os_str().is_empty())
    {
        std::fs::create_dir_all(directory)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).context("Opening bounded daemon log")?;
    if !file.metadata()?.is_file() {
        bail!("Daemon log must be a regular file");
    }
    let mut sink = SINK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow::anyhow!("Daemon logging lock poisoned"))?;
    *sink = Some(Sink {
        file,
        servers,
        identities: BTreeMap::new(),
    });
    crate::diagnostics::set_enabled(true);
    Ok(())
}

pub(crate) fn register_identity(runtime: &str, configured: &str) -> Result<()> {
    let mut sink = SINK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| anyhow::anyhow!("Daemon logging lock poisoned"))?;
    if let Some(sink) = sink.as_mut() {
        if sink
            .identities
            .get(runtime)
            .is_some_and(|previous| previous != configured)
        {
            bail!("Conflicting logical identity for an existing pool log key");
        }
        sink.identities
            .insert(runtime.to_owned(), configured.to_owned());
    }
    Ok(())
}

pub(super) fn logical_name(runtime: &str) -> String {
    SINK.get_or_init(|| Mutex::new(None))
        .lock()
        .ok()
        .and_then(|sink| {
            sink.as_ref()
                .and_then(|sink| sink.identities.get(runtime).cloned())
        })
        .unwrap_or_else(|| runtime.to_owned())
}

pub(crate) fn write(server: Option<&str>, message: &str) -> Option<std::io::Result<()>> {
    let mut sink = match SINK.get_or_init(|| Mutex::new(None)).lock() {
        Ok(sink) => sink,
        Err(_) => return Some(Err(std::io::Error::other("Daemon logging lock poisoned"))),
    };
    let sink = sink.as_mut()?;
    if !selected(server, &sink.servers) {
        return Some(Ok(()));
    }
    if super::STDERR.load(std::sync::atomic::Ordering::SeqCst) {
        eprintln!("{message}");
    }
    Some(append_bounded(
        &mut sink.file,
        &crate::diagnostics::summarize_log_line(message),
    ))
}

fn selected(server: Option<&str>, servers: &BTreeSet<String>) -> bool {
    servers.is_empty() || server.is_none_or(|server| servers.contains(server))
}

fn append_bounded(file: &mut std::fs::File, message: &str) -> std::io::Result<()> {
    if file
        .metadata()?
        .len()
        .saturating_add(message.len() as u64 + 1)
        > MAX_BYTES
    {
        let length = file.metadata()?.len();
        file.seek(SeekFrom::Start(length.saturating_sub(MAX_BYTES / 2)))?;
        let mut tail = Vec::new();
        file.take(MAX_BYTES / 2).read_to_end(&mut tail)?;
        if let Some(newline) = tail.iter().position(|byte| *byte == b'\n') {
            tail.drain(..=newline);
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&tail)?;
    }
    file.seek(SeekFrom::End(0))?;
    writeln!(file, "{message}")?;
    file.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn operation_log_remains_bounded_after_rotation() -> Result<()> {
        let directory = std::env::current_dir()?.join("target");
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(format!("logging-fixture-{}.log", std::process::id()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .open(&path)?;
            file.write_all(&vec![b'x'; MAX_BYTES as usize])?;
            append_bounded(&mut file, "operation complete")?;
            assert!(file.metadata()?.len() < MAX_BYTES);
            Ok(())
        })();
        std::fs::remove_file(path)?;
        result
    }

    #[test]
    fn filters_select_exact_identity_and_keep_generic_records() {
        let servers = ["a-long-configured-name".to_owned()].into();
        assert!(selected(Some("a-long-configured-name"), &servers));
        assert!(!selected(Some("a-long-configured-name-extra"), &servers));
        assert!(!selected(Some("mcp-a-long-0123456789"), &servers));
        assert!(selected(None, &servers));
    }
}
