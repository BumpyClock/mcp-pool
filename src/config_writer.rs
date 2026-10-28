use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

#[path = "config_permissions.rs"]
mod permissions;

struct Guard {
    path: PathBuf,
    file: Option<std::fs::File>,
}

pub(crate) fn read_document(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(contents) => parse_document(&contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(json!({"mcpServers":{}})),
        Err(error) => Err(error).context("Reading MCP config"),
    }
}

fn parse_document(contents: &str) -> Result<Value> {
    jsonc_parser::parse_to_serde_value(
        contents.trim_start_matches('\u{feff}'),
        &jsonc_parser::ParseOptions::default(),
    )
    .map_err(|_| anyhow!("Config is not valid JSONC; no changes made"))?
    .context("Config is empty; no changes made")
}

impl Drop for Guard {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Err(error) = std::fs::remove_file(&self.path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("[mcp-pool] Could not remove config write guard: {error}");
        }
    }
}

pub(crate) async fn write_checked_mutation<F, Before>(
    path: &Path,
    mutate: impl FnOnce(&mut Value) -> Result<()>,
    before: F,
) -> Result<()>
where
    F: FnOnce(Value) -> Before,
    Before: std::future::Future<Output = Result<()>>,
{
    let directory = path.parent().context("Config path has no directory")?;
    crate::oauth::files::create_directory(directory)?;
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        bail!("Refusing to replace a symlink config; use its real path explicitly");
    }
    let lock_path = path.with_extension("json.lock");
    let mut acquired = None;
    for _ in 0..40 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock_path)
        {
            Ok(file) => {
                acquired = Some(file);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                tokio::time::sleep(Duration::from_millis(50)).await
            }
            Err(error) => return Err(error).context("Acquiring config write lock"),
        }
    }
    let lock_file = acquired.context("Config is being changed by another process; retry later")?;
    let lock_guard = Guard {
        path: lock_path,
        file: Some(lock_file),
    };
    let original = match std::fs::read_to_string(path) {
        Ok(contents) => Some(contents),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("Reading config before mutation"),
    };
    let permissions = if original.is_some() {
        Some(permissions::Snapshot::read(path)?)
    } else {
        None
    };
    let mut document = if let Some(contents) = &original {
        parse_document(contents)?
    } else {
        json!({"mcpServers":{}})
    };
    let previous = document.clone();
    mutate(&mut document)?;
    let contents = serde_json::to_string_pretty(&document)?;
    crate::server_config::validate_document(&document)?;
    let staging = path.with_extension(format!("json.write-{}", std::process::id()));
    let mut file = crate::oauth::files::private_file(&staging)
        .context("Creating atomic config replacement")?;
    let staging_guard = Guard {
        path: staging.clone(),
        file: None,
    };
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    if let Some(permissions) = &permissions {
        permissions.apply(&file)?;
    }
    drop(file);
    drop(permissions);
    before(previous).await?;
    let outcome = (|| {
        let current = match std::fs::read_to_string(path) {
            Ok(contents) => Some(contents),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("Checking concurrent config changes"),
        };
        if current != original {
            bail!("Config changed during mutation; no changes made, retry");
        }
        std::fs::rename(&staging, path).context("Replacing config atomically")?;
        Ok(())
    })();
    drop(staging_guard);
    drop(lock_guard);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn failed_retirement_keeps_original_file_and_cleans_staging() -> Result<()> {
        let identity = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let directory = std::env::current_dir()?
            .join("target")
            .join(format!("writer-retirement-{identity}"));
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("config.json");
        let original = r#"{"imports":[],"mcpServers":{"fixture":{"command":"echo"}},"custom":4}"#;
        let result = async {
            std::fs::write(&path, original)?;
            let denied = write_checked_mutation(
                &path,
                |document| {
                    document
                        .as_object_mut()
                        .context("object")?
                        .insert("mcpServers".to_owned(), json!({}));
                    Ok(())
                },
                |_| async { bail!("unverified retirement") },
            )
            .await;
            assert!(denied.is_err());
            assert_eq!(std::fs::read_to_string(&path)?, original);
            assert!(!path.with_extension("json.lock").exists());
            assert!(
                !path
                    .with_extension(format!("json.write-{}", std::process::id()))
                    .exists()
            );
            Ok::<(), anyhow::Error>(())
        }
        .await;
        std::fs::remove_dir_all(directory)?;
        result
    }

    #[tokio::test]
    async fn invalid_legacy_imports_prevent_atomic_replacement() -> Result<()> {
        let directory = std::env::current_dir()?.join("target").join(format!(
            "writer-imports-{}",
            oauth2::CsrfToken::new_random().secret()
        ));
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("config.json");
        let result = async {
            for imports in [json!("not-an-array"), json!([3])] {
                let original =
                    json!({"mcpServers":{"fixture":{"command":"echo"}},"imports":imports})
                        .to_string();
                std::fs::write(&path, &original)?;
                let denied = write_checked_mutation(
                    &path,
                    |document| {
                        document
                            .as_object_mut()
                            .context("object")?
                            .insert("custom".into(), json!(4));
                        Ok(())
                    },
                    |_| async { Ok(()) },
                )
                .await;
                assert!(denied.is_err(), "Malformed config must not be committed");
                assert_eq!(std::fs::read_to_string(&path)?, original);
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        std::fs::remove_dir_all(directory)?;
        result
    }
}
