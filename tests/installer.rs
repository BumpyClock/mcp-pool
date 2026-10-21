#[cfg(windows)]
mod windows {
    use std::io;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::process::Command;

    struct Fixture(PathBuf);

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Err(error) = std::fs::remove_dir_all(&self.0) {
                eprintln!(
                    "Could not remove installer fixture {}: {error}",
                    self.0.display()
                );
            }
        }
    }

    #[tokio::test]
    async fn installer_replaces_an_existing_executable_from_another_directory() -> io::Result<()> {
        let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let directory = repository
            .join("target")
            .join(format!("installer-fixture-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&directory)?;
        let fixture = Fixture(directory);
        let source = fixture
            .0
            .join("target")
            .join("fixture-host")
            .join("release")
            .join("mcp-pool.exe");
        let destination = fixture.0.join("bin").join("mcp-pool.exe");
        let other_directory = fixture.0.join("other working directory");
        std::fs::create_dir_all(
            source
                .parent()
                .ok_or_else(|| io::Error::other("source parent"))?,
        )?;
        std::fs::create_dir_all(
            destination
                .parent()
                .ok_or_else(|| io::Error::other("destination parent"))?,
        )?;
        std::fs::create_dir(&other_directory)?;
        std::fs::copy(env!("CARGO_BIN_EXE_mcp-pool"), &source)?;
        std::fs::write(&destination, b"previous executable")?;
        std::fs::copy(
            repository.join("install.ps1"),
            fixture.0.join("install.ps1"),
        )?;
        std::fs::write(fixture.0.join("Cargo.toml"), b"mock build input")?;

        let script = r#"
$ErrorActionPreference = 'Stop'
function global:rustc {
    if ($args.Count -ne 1 -or $args[0] -ne '-vV') { throw 'Unexpected rustc invocation' }
    'host: fixture-host'
    $global:LASTEXITCODE = 0
}
function global:cargo {
    $expectedManifest = Join-Path $env:MCP_INSTALLER_FIXTURE 'Cargo.toml'
    $expectedTarget = Join-Path $env:MCP_INSTALLER_FIXTURE 'target'
    if ($args[0] -ne 'build' -or
        $args[1] -ne '--manifest-path' -or $args[2] -ne $expectedManifest -or
        $args[3] -ne '--target-dir' -or $args[4] -ne $expectedTarget -or
        $args[5] -ne '--target' -or $args[6] -ne 'fixture-host' -or
        $args[7] -ne '--release' -or $args[8] -ne '--locked') {
        throw 'Installer did not build its own manifest and host target'
    }
    $global:LASTEXITCODE = 0
}
& (Join-Path $env:MCP_INSTALLER_FIXTURE 'install.ps1') -BinDir (Join-Path $env:MCP_INSTALLER_FIXTURE 'bin')
if (-not $?) { throw 'Installer failed' }
"#;
        let output = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .env("MCP_INSTALLER_FIXTURE", &fixture.0)
            // Let Windows PowerShell discover its own modules, not PowerShell 7 modules.
            .env_remove("PSModulePath")
            .current_dir(&other_directory)
            .output()
            .await?;
        assert!(
            output.status.success(),
            "Installer failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(&destination)?, std::fs::read(&source)?);
        let installed = Command::new(&destination).arg("--version").output().await?;
        assert!(installed.status.success());
        assert!(
            String::from_utf8_lossy(&installed.stdout).contains(env!("MCP_POOL_BUILD_VERSION"))
        );
        let staging_files =
            std::fs::read_dir(fixture.0.join("bin"))?.collect::<io::Result<Vec<_>>>()?;
        assert_eq!(staging_files.len(), 1, "Installer left a staged executable");
        Ok(())
    }
}
