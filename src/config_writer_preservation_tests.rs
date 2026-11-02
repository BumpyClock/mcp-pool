use std::cell::Cell;
use std::path::PathBuf;

use super::*;

struct Fixture {
    directory: PathBuf,
    path: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let directory = std::env::current_dir()?.join("target").join(format!(
            "writer-preserve-{}",
            oauth2::CsrfToken::new_random().secret()
        ));
        std::fs::create_dir_all(&directory)?;
        Ok(Self {
            path: directory.join("config.json"),
            directory,
        })
    }

    fn assert_clean(&self) {
        assert!(!self.path.with_extension("json.lock").exists());
        assert!(!self.staging().exists());
    }

    fn staging(&self) -> PathBuf {
        self.path
            .with_extension(format!("json.write-{}", std::process::id()))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory) {
            eprintln!("Could not remove writer fixture: {error}");
        }
    }
}

#[tokio::test]
async fn preservation_runs_callback_under_lock_without_staging() -> Result<()> {
    let fixture = Fixture::new()?;
    let original = "\u{feff}{ // keep\n \"mcpServers\": {\"local\":{\"command\":\"node\"}}\n}\n";
    std::fs::write(&fixture.path, original)?;
    let invoked = Cell::new(false);
    write_checked_mutation(
        &fixture.path,
        WritePolicy::PreserveUnchanged,
        |_| Ok(()),
        |previous| {
            assert!(fixture.path.with_extension("json.lock").exists());
            assert!(!fixture.staging().exists());
            assert_eq!(previous, json!({"mcpServers":{"local":{"command":"node"}}}));
            invoked.set(true);
            async { Ok(()) }
        },
    )
    .await?;
    assert!(invoked.get());
    assert_eq!(std::fs::read_to_string(&fixture.path)?, original);
    fixture.assert_clean();
    Ok(())
}

#[tokio::test]
async fn unchanged_invalid_documents_still_fail_validation_before_callback() -> Result<()> {
    let fixture = Fixture::new()?;
    for document in [
        json!({"imports":"invalid","mcpServers":{}}),
        json!({"mcpServers":{"invalid":{"command":""}}}),
    ] {
        let original = document.to_string();
        std::fs::write(&fixture.path, &original)?;
        let invoked = Cell::new(false);
        let denied = write_checked_mutation(
            &fixture.path,
            WritePolicy::PreserveUnchanged,
            |_| Ok(()),
            |_| {
                invoked.set(true);
                async { Ok(()) }
            },
        )
        .await;
        assert!(denied.is_err());
        assert!(!invoked.get());
        assert_eq!(std::fs::read_to_string(&fixture.path)?, original);
        fixture.assert_clean();
    }
    Ok(())
}

#[tokio::test]
async fn unchanged_callback_failures_are_not_successful_noops() -> Result<()> {
    let fixture = Fixture::new()?;
    let original = r#"{"mcpServers":{}}"#;
    std::fs::write(&fixture.path, original)?;
    let denied = write_checked_mutation(
        &fixture.path,
        WritePolicy::PreserveUnchanged,
        |_| Ok(()),
        |_| async { bail!("unverified auth retirement") },
    )
    .await;
    assert!(
        denied
            .err()
            .context("expected callback failure")?
            .to_string()
            .contains("unverified auth retirement")
    );
    assert_eq!(std::fs::read_to_string(&fixture.path)?, original);
    fixture.assert_clean();
    Ok(())
}

#[tokio::test]
async fn callbacks_cannot_hide_external_byte_changes_under_either_policy() -> Result<()> {
    let fixture = Fixture::new()?;
    for policy in [WritePolicy::PreserveUnchanged, WritePolicy::Replace] {
        std::fs::write(&fixture.path, r#"{"mcpServers":{}}"#)?;
        let external = "{ // External formatting edit.\n\"mcpServers\":{}\n}";
        let denied = write_checked_mutation(
            &fixture.path,
            policy,
            |_| Ok(()),
            |_| async {
                std::fs::write(&fixture.path, external)?;
                Ok(())
            },
        )
        .await;
        assert!(
            denied
                .err()
                .context("expected concurrent edit rejection")?
                .to_string()
                .contains("Config changed during mutation")
        );
        assert_eq!(std::fs::read_to_string(&fixture.path)?, external);
        fixture.assert_clean();
    }
    Ok(())
}

#[tokio::test]
async fn preservation_still_creates_missing_destinations_and_stages_real_changes() -> Result<()> {
    let fixture = Fixture::new()?;
    write_checked_mutation(
        &fixture.path,
        WritePolicy::PreserveUnchanged,
        |_| Ok(()),
        |_| async { Ok(()) },
    )
    .await?;
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(&fixture.path)?)?,
        json!({"mcpServers":{}})
    );
    let original = std::fs::read(&fixture.path)?;
    let denied = write_checked_mutation(
        &fixture.path,
        WritePolicy::PreserveUnchanged,
        |document| {
            document
                .as_object_mut()
                .context("document")?
                .insert("custom".to_owned(), json!(true));
            Ok(())
        },
        |_| async {
            assert!(fixture.staging().exists());
            bail!("unverified replacement retirement")
        },
    )
    .await;
    assert!(denied.is_err());
    assert_eq!(std::fs::read(&fixture.path)?, original);
    fixture.assert_clean();
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn preservation_keeps_file_identity_and_replacement_keeps_permissions() -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let fixture = Fixture::new()?;
    std::fs::write(&fixture.path, "{ \"mcpServers\": {} }\n")?;
    std::fs::set_permissions(&fixture.path, std::fs::Permissions::from_mode(0o640))?;
    let original = std::fs::metadata(&fixture.path)?;
    write_checked_mutation(
        &fixture.path,
        WritePolicy::PreserveUnchanged,
        |_| Ok(()),
        |_| async { Ok(()) },
    )
    .await?;
    assert_eq!(std::fs::metadata(&fixture.path)?.ino(), original.ino());
    write_checked_mutation(
        &fixture.path,
        WritePolicy::PreserveUnchanged,
        |document| {
            document
                .as_object_mut()
                .context("document")?
                .insert("custom".to_owned(), json!(true));
            Ok(())
        },
        |_| async { Ok(()) },
    )
    .await?;
    assert_eq!(
        std::fs::metadata(&fixture.path)?.permissions().mode() & 0o777,
        0o640
    );
    fixture.assert_clean();
    Ok(())
}

#[tokio::test]
async fn unchanged_readonly_configs_still_fail_permission_policy() -> Result<()> {
    let fixture = Fixture::new()?;
    std::fs::write(&fixture.path, r#"{"mcpServers":{}}"#)?;
    let original_permissions = std::fs::metadata(&fixture.path)?.permissions();
    let mut readonly = original_permissions.clone();
    readonly.set_readonly(true);
    std::fs::set_permissions(&fixture.path, readonly)?;
    let denied = write_checked_mutation(
        &fixture.path,
        WritePolicy::PreserveUnchanged,
        |_| Ok(()),
        |_| async { Ok(()) },
    )
    .await;
    std::fs::set_permissions(&fixture.path, original_permissions)?;
    assert!(
        denied
            .err()
            .context("expected read-only rejection")?
            .to_string()
            .contains("Config is read-only")
    );
    fixture.assert_clean();
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn unchanged_symlink_configs_are_still_refused() -> Result<()> {
    let fixture = Fixture::new()?;
    let target = fixture.directory.join("target.json");
    let original = r#"{"mcpServers":{}}"#;
    std::fs::write(&target, original)?;
    std::os::unix::fs::symlink(&target, &fixture.path)?;
    let denied = write_checked_mutation(
        &fixture.path,
        WritePolicy::PreserveUnchanged,
        |_| Ok(()),
        |_| async { Ok(()) },
    )
    .await;
    assert!(
        denied
            .err()
            .context("expected symlink rejection")?
            .to_string()
            .contains("Refusing to replace a symlink")
    );
    assert_eq!(std::fs::read_to_string(target)?, original);
    fixture.assert_clean();
    Ok(())
}
