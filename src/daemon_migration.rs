use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[path = "legacy_directories.rs"]
mod directories;
#[path = "process_snapshot.rs"]
mod process;

struct Legacy {
    pid: u32,
    socket: PathBuf,
    directory: PathBuf,
    verified: bool,
}

pub(super) async fn run(arguments: VecDeque<String>) -> Result<()> {
    let stop = arguments.iter().any(|argument| argument == "--stop-legacy");
    let confirmed = arguments
        .iter()
        .any(|argument| argument == "--confirmed-drained");
    let json_output = arguments.iter().any(|argument| argument == "--json");
    for argument in &arguments {
        if !matches!(
            argument.as_str(),
            "--stop-legacy" | "--confirmed-drained" | "--json"
        ) {
            bail!("Unknown daemon migrate flag '{argument}'");
        }
    }
    if stop && !confirmed {
        bail!(
            "Drain all legacy clients first, then pass --confirmed-drained. No process was stopped."
        );
    }
    if confirmed && !stop {
        bail!("--confirmed-drained requires --stop-legacy");
    }
    let selected = directories::directories()?;
    let existing = tokio::task::spawn_blocking(move || -> Result<Vec<PathBuf>> {
        let mut existing = Vec::new();
        for directory in selected {
            if directories::inspect(&directory)? {
                existing.push(directory);
            }
        }
        Ok(existing)
    })
    .await
    .context("Legacy directory inspection task failed")??;
    if existing.is_empty() {
        if stop && !json_output {
            println!("No legacy daemon directories or owners remain.");
        } else {
            println!("[]");
        }
        return Ok(());
    }
    for directory in &existing {
        let marker = directory.join("legacy-retirement.json");
        match tokio::fs::symlink_metadata(&marker).await {
            Ok(_) if !stop => bail!(
                "Legacy retirement remains unverified; use daemon migrate --stop-legacy --confirmed-drained to verify the saved identities"
            ),
            Ok(_) => {
                directories::inspect_file(&marker)?;
                let saved: Value = serde_json::from_slice(&tokio::fs::read(&marker).await?)?;
                if saved.get("format").and_then(Value::as_str) != Some("mcp-pool-legacy-v1") {
                    bail!(
                        "Unrecognized legacy retirement journal; inspect it with its owning mcporter version before cutover"
                    );
                }
                let identities: Vec<process::Identity> = serde_json::from_value(
                    saved
                        .get("processes")
                        .cloned()
                        .context("Retirement journal omitted process identities")?,
                )?;
                verify_retirement(&identities).await?;
                tokio::fs::remove_file(marker).await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("Inspecting legacy retirement journal"),
        }
    }
    let snapshot = process::snapshot().await?;
    let mut legacy = Vec::new();
    for directory in &existing {
        let mut entries = tokio::fs::read_dir(directory).await?;
        while let Some(entry) = entries.next_entry().await? {
            let filename = entry.file_name().to_string_lossy().into_owned();
            let Some(key) = legacy_key(&filename) else {
                continue;
            };
            let path = entry.path();
            directories::inspect_file(&path)?;
            let metadata: Value = serde_json::from_slice(&tokio::fs::read(path).await?)
                .context("Malformed legacy metadata; inspect it manually before cutover")?;
            let pid = metadata
                .get("pid")
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| *pid > 0)
                .context("Legacy metadata omitted a valid PID")?;
            let recorded = metadata
                .get("socketPath")
                .and_then(Value::as_str)
                .context("Legacy metadata omitted socketPath")?;
            if !snapshot.processes.iter().any(|process| process.pid == pid) {
                continue;
            }
            #[cfg(windows)]
            let socket = PathBuf::from(format!(r"\\.\pipe\mcporter-daemon-{key}"));
            #[cfg(unix)]
            let socket = directory.join(format!("daemon-{key}.sock"));
            let live = if recorded == socket.to_string_lossy() {
                probe(&socket, "status").await?
            } else {
                None
            };
            let verified = matches_status(live.as_ref(), pid, &socket)
                && process::owned_tree(&snapshot, pid).is_ok();
            legacy.push(Legacy {
                pid,
                socket,
                directory: directory.clone(),
                verified,
            });
        }
    }
    legacy.sort_by_key(|entry| entry.pid);
    if !stop {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &legacy
                    .iter()
                    .map(|entry| json!({"pid":entry.pid,"verified":entry.verified}))
                    .collect::<Vec<_>>()
            )?
        );
        return Ok(());
    }
    if legacy.iter().any(|entry| !entry.verified) {
        bail!("Legacy process ownership is unverified; no process was stopped");
    }
    for directory in &existing {
        let directory = directory.clone();
        tokio::task::spawn_blocking(move || directories::upgrade(&directory))
            .await
            .context("Legacy permission upgrade task failed")??;
    }
    for entry in &legacy {
        if !matches_status(
            probe(&entry.socket, "status").await?.as_ref(),
            entry.pid,
            &entry.socket,
        ) {
            bail!("Legacy owner changed during cutover; no replacement was started");
        }
        let snapshot = process::snapshot().await?;
        let identities = process::owned_tree(&snapshot, entry.pid)?;
        let marker = entry.directory.join("legacy-retirement.json");
        save_marker(&marker, &identities)?;
        if probe(&entry.socket, "stop").await? != Some(Value::Bool(true)) {
            bail!("Verified legacy daemon did not acknowledge stop; retirement journal retained");
        }
        verify_retirement(&identities).await?;
        tokio::fs::remove_file(marker).await?;
    }
    if json_output {
        println!("{}", json!({"retired":legacy.len(),"verified":true}));
    } else {
        println!(
            "Legacy directory permissions verified; {} observed daemon process trees retired.",
            legacy.len()
        );
    }
    Ok(())
}

fn legacy_key(filename: &str) -> Option<&str> {
    filename
        .strip_prefix("daemon-")?
        .strip_suffix(".json")
        .filter(|key| {
            !key.is_empty()
                && key
                    .chars()
                    .all(|character| character.is_ascii_digit() || matches!(character, 'a'..='f'))
        })
}

fn matches_status(status: Option<&Value>, pid: u32, socket: &Path) -> bool {
    status.is_some_and(|status| {
        status.get("pid").and_then(Value::as_u64) == Some(u64::from(pid))
            && status.get("socketPath").and_then(Value::as_str)
                == Some(socket.to_string_lossy().as_ref())
            && status
                .get("protocolVersion")
                .and_then(Value::as_u64)
                .unwrap_or(1)
                < 3
    })
}

fn save_marker(path: &Path, identities: &[process::Identity]) -> Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .context("Saving verified retirement identities")?;
    file.write_all(&serde_json::to_vec(
        &json!({"format":"mcp-pool-legacy-v1","processes":identities}),
    )?)?;
    file.sync_all()?;
    Ok(())
}

async fn verify_retirement(identities: &[process::Identity]) -> Result<()> {
    if identities.is_empty() {
        bail!("Retirement journal contains no verified identities");
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        if process::remaining(&process::snapshot().await?, identities)?.is_empty() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "Legacy process-tree retirement was not verified within 15 seconds. Journal retained; stop remaining workers through their owning runtime before retrying. No PID was signaled."
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn probe(path: &Path, method: &str) -> Result<Option<Value>> {
    let operation = async {
        #[cfg(unix)]
        let mut stream: crate::transport::LocalStream = {
            use std::os::unix::fs::{FileTypeExt, MetadataExt};
            let metadata = match std::fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error).context("Inspecting legacy socket"),
            };
            if !metadata.file_type().is_socket() || metadata.uid() != unsafe { libc::geteuid() } {
                bail!("Legacy socket ownership is unverified");
            }
            let stream = match tokio::net::UnixStream::connect(path).await {
                Ok(stream) => stream,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) => return Err(error).context("Connecting to legacy daemon"),
            };
            crate::local_security::verify_unix_peer(&stream)?;
            Box::new(stream)
        };
        #[cfg(windows)]
        let mut stream = match crate::transport::connect(path).await {
            Ok(stream) => stream,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(error).context("Verifying legacy named-pipe owner"),
        };
        stream.write_all(serde_json::to_string(&json!({"id":format!("mcp-pool-migration-{}",std::process::id()),"method":method,"params":{}}))?.as_bytes()).await?;
        stream.flush().await?;
        let mut content = Vec::new();
        let mut buffer = vec![0u8; 4096];
        loop {
            let count = stream.read(&mut buffer).await?;
            if count == 0 {
                return Ok(None);
            }
            content.extend_from_slice(
                buffer
                    .get(..count)
                    .context("Invalid legacy response size")?,
            );
            if content.len() > 1024 * 1024 {
                bail!("Legacy response exceeds 1 MiB");
            }
            match serde_json::from_slice::<Value>(&content) {
                Ok(response) => {
                    return Ok(if response.get("ok") == Some(&Value::Bool(true)) {
                        response.get("result").cloned()
                    } else {
                        None
                    });
                }
                Err(error) if error.is_eof() => continue,
                Err(_) => bail!("Legacy daemon returned invalid JSON"),
            }
        }
    };
    match tokio::time::timeout(Duration::from_secs(1), operation).await {
        Ok(result) => result,
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn migration_requires_drain_confirmation_before_any_inspection() {
        assert!(run(["--stop-legacy".to_owned()].into()).await.is_err());
        assert!(
            run(["--confirmed-drained".to_owned()].into())
                .await
                .is_err()
        );
    }
    #[test]
    fn legacy_metadata_and_protocol_identity_must_match() {
        assert_eq!(legacy_key("daemon-123abc.json"), Some("123abc"));
        assert_eq!(legacy_key("daemon-../../user.json"), None);
        assert_eq!(legacy_key("daemon-.json"), None);
        let socket = Path::new("synthetic-socket");
        assert!(matches_status(
            Some(&json!({"pid":10,"socketPath":"synthetic-socket","protocolVersion":2})),
            10,
            socket
        ));
        assert!(!matches_status(
            Some(&json!({"pid":11,"socketPath":"synthetic-socket","protocolVersion":2})),
            10,
            socket
        ));
        assert!(!matches_status(
            Some(&json!({"pid":10,"socketPath":"synthetic-socket","protocolVersion":3})),
            10,
            socket
        ));
    }
}
