use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine;
use serde_json::Value;

pub(super) async fn save_images(result: &Value, directory: Option<&Path>) -> Result<()> {
    let Some(directory) = directory else {
        return Ok(());
    };
    let directory = directory.to_path_buf();
    let content = result
        .get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let paths = tokio::task::spawn_blocking(move || save(&directory, &content))
        .await
        .context("Image write task failed")??;
    for path in paths {
        eprintln!("[mcp-cli] Saved image: {}", path.display());
    }
    Ok(())
}

fn save(directory: &Path, content: &[Value]) -> Result<Vec<PathBuf>> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    save_at(directory, content, timestamp)
}

fn save_at(directory: &Path, content: &[Value], timestamp: u128) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for (index, image) in content
        .iter()
        .enumerate()
        .filter(|(_, value)| value.get("type").and_then(Value::as_str) == Some("image"))
    {
        let data = image
            .get("data")
            .and_then(Value::as_str)
            .context("Image content omitted base64 data")?;
        if data.len() > 24 * 1024 * 1024 {
            bail!("Image content exceeds the 16 MiB decoded limit");
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("Image content contains invalid base64")?;
        if data.len() > 16 * 1024 * 1024 {
            bail!("Image content exceeds the 16 MiB decoded limit");
        }
        let extension = match image.get("mimeType").and_then(Value::as_str).unwrap_or("") {
            "image/png" => "png",
            "image/jpeg" | "image/jpg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "image/svg+xml" => "svg",
            "image/bmp" => "bmp",
            "image/tiff" => "tiff",
            "image/x-icon" | "image/vnd.microsoft.icon" => "ico",
            _ => "bin",
        };
        std::fs::create_dir_all(directory).context("Creating image output directory")?;
        let mut saved = None;
        for attempt in 0..100u32 {
            let path = directory.join(format!(
                "mcp-image-{timestamp}-{index}-{attempt}.{extension}"
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(&data).and_then(|_| file.sync_all()) {
                        drop(file);
                        if let Err(cleanup) = std::fs::remove_file(&path) {
                            eprintln!("[mcp-cli] Could not clean incomplete image: {cleanup}");
                        }
                        return Err(error).context("Writing image content");
                    }
                    saved = Some(path);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error).context("Creating image output file"),
            }
        }
        paths.push(saved.context("Could not allocate a collision-free image filename")?);
    }
    Ok(paths)
}

pub(super) async fn tail_log(result: &Value) -> Result<()> {
    let path = ["logPath", "logFile", "logfile"]
        .iter()
        .find_map(|key| result.get(key).and_then(Value::as_str));
    let Some(path) = path else {
        eprintln!("[mcp-cli] --tail-log: result has no log path");
        return Ok(());
    };
    let path = PathBuf::from(path);
    let selected = path.clone();
    let tail = tokio::task::spawn_blocking(move || read_tail(&selected))
        .await
        .context("Log tail task failed")?;
    match tail {
        Ok(lines) => {
            println!("--- tail {} ---", path.display());
            println!("{lines}");
        }
        Err(error) => eprintln!("[mcp-cli] --tail-log: {error}"),
    }
    Ok(())
}

fn read_tail(path: &Path) -> Result<String> {
    if !path.is_absolute() {
        bail!("Refusing a nonabsolute log path");
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let mut file = options.open(path).context("Opening result log")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        bail!("Result log must be a regular file");
    }
    let offset = metadata.len().saturating_sub(1024 * 1024);
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::new();
    file.take(1024 * 1024).read_to_end(&mut bytes)?;
    let contents = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<_> = contents.lines().collect();
    if offset > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    Ok(lines
        .get(lines.len().saturating_sub(20)..)
        .unwrap_or_default()
        .join("\n"))
}

#[cfg(test)]
#[path = "tool_artifact_tests.rs"]
mod tests;
