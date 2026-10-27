#![cfg(unix)]

use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::process::Command;

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            eprintln!("Could not remove Unix installer fixture: {error}");
        }
    }
}

#[tokio::test]
async fn installer_atomically_replaces_an_executable_and_preserves_it_on_build_failure()
-> io::Result<()> {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    let fixture = Fixture(
        std::env::temp_dir()
            .canonicalize()?
            .join(format!("unix-installer-{}-{nonce}", std::process::id())),
    );
    let source = fixture
        .0
        .join("target")
        .join("fixture-host")
        .join("release")
        .join("mcp-pool");
    let destination = fixture.0.join("bin").join("mcp-pool");
    let tools = fixture.0.join("mock-tools");
    let other_directory = fixture.0.join("other working directory");
    for directory in [
        source.parent(),
        destination.parent(),
        Some(tools.as_path()),
        Some(other_directory.as_path()),
    ] {
        std::fs::create_dir_all(
            directory.ok_or_else(|| io::Error::other("fixture directory has no parent"))?,
        )?;
    }
    std::fs::copy(env!("CARGO_BIN_EXE_mcp-pool"), &source)?;
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o751))?;
    std::fs::write(&destination, b"previous executable")?;
    std::fs::copy(repository.join("install.sh"), fixture.0.join("install.sh"))?;
    std::fs::write(fixture.0.join("Cargo.toml"), b"mock build input")?;
    std::fs::write(
        tools.join("rustc"),
        "#!/bin/sh\nprintf '%s\\n' 'host: fixture-host'\n",
    )?;
    std::fs::write(
        tools.join("cargo"),
        r#"#!/bin/sh
set -eu
[ "${MCP_INSTALLER_BUILD_FAILURE:-0}" = 0 ] || exit 1
[ "$PWD" = "$MCP_INSTALLER_FIXTURE" ]
[ "$#" = 9 ]
[ "$1" = build ]
[ "$2" = --manifest-path ] && [ "$3" = "$MCP_INSTALLER_FIXTURE/Cargo.toml" ]
[ "$4" = --target-dir ] && [ "$5" = "$MCP_INSTALLER_FIXTURE/target" ]
[ "$6" = --target ] && [ "$7" = fixture-host ]
[ "$8" = --release ] && [ "$9" = --locked ]
"#,
    )?;
    for tool in ["rustc", "cargo"] {
        std::fs::set_permissions(tools.join(tool), std::fs::Permissions::from_mode(0o755))?;
    }
    let mut paths = vec![tools];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).map_err(io::Error::other)?;
    for failure in ["0", "1"] {
        let output = Command::new("sh")
            .arg(fixture.0.join("install.sh"))
            .arg("--bin-dir")
            .arg(
                destination
                    .parent()
                    .ok_or_else(|| io::Error::other("bin directory"))?,
            )
            .env("MCP_INSTALLER_FIXTURE", &fixture.0)
            .env("MCP_INSTALLER_BUILD_FAILURE", failure)
            .env("PATH", &path)
            .current_dir(&other_directory)
            .output()
            .await?;
        assert_eq!(
            output.status.success(),
            failure == "0",
            "Installer output: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(&destination)?, std::fs::read(&source)?);
        assert_eq!(
            std::fs::metadata(&destination)?.permissions().mode() & 0o777,
            0o751
        );
        assert_eq!(
            std::fs::read_dir(fixture.0.join("bin"))?
                .collect::<io::Result<Vec<_>>>()?
                .len(),
            1,
            "Staging file was not cleaned"
        );
    }
    let installed = Command::new(&destination).arg("--version").output().await?;
    assert!(installed.status.success());
    assert!(String::from_utf8_lossy(&installed.stdout).contains(env!("MCP_POOL_BUILD_VERSION")));
    Ok(())
}
