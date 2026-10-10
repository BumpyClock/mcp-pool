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
        eprintln!("[mcp-pool] Saved image: {}", path.display());
    }
    Ok(())
}

fn save(directory: &Path, content: &[Value]) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
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
                            eprintln!("[mcp-pool] Could not clean incomplete image: {cleanup}");
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
        eprintln!("[mcp-pool] --tail-log: result has no log path");
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
        Err(error) => eprintln!("[mcp-pool] --tail-log: {error}"),
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
mod tests {
    use super::*;
    #[test]
    fn image_and_log_artifacts_are_bounded_and_do_not_overwrite() -> Result<()> {
        let directory = std::env::current_dir()?
            .join("target")
            .join(format!("artifact-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let result = (|| {
            let images =
                vec![serde_json::json!({"type":"image","data":"aGVsbG8=","mimeType":"image/png"})];
            let first = save(&directory, &images)?;
            let second = save(&directory, &images)?;
            assert_ne!(first, second);
            assert_eq!(
                std::fs::read(first.first().context("image path")?)?,
                b"hello"
            );
            let log = directory.join("fixture.log");
            std::fs::write(
                &log,
                (0..30)
                    .map(|index| index.to_string())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )?;
            let tail = read_tail(&log)?;
            assert_eq!(tail.lines().count(), 20);
            assert!(tail.starts_with("10\n"));
            for contents in ["", "one", "one\ntwo\n"] {
                std::fs::write(&log, contents)?;
                assert_eq!(read_tail(&log)?, contents.trim_end_matches('\n'));
            }
            std::fs::write(&log, format!("{}partial\nlast\n", "x".repeat(1024 * 1024)))?;
            assert_eq!(read_tail(&log)?, "last");
            assert!(read_tail(Path::new("relative.log")).is_err());
            Ok(())
        })();
        std::fs::remove_dir_all(directory)?;
        result
    }
}
