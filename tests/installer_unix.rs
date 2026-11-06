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
async fn installer_atomically_replaces_an_executable_and_preserves_it_on_verification_failure()
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
        .join("mcp-cli");
    let destination = fixture.0.join("bin").join("mcp-cli");
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
    std::fs::copy(env!("CARGO_BIN_EXE_mcp-cli"), &source)?;
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
    let real_hash = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .flat_map(|directory| [directory.join("sha256sum"), directory.join("shasum")])
        .find(|candidate| candidate.is_file())
        .ok_or_else(|| io::Error::other("Prepare sha256sum or shasum for installer fixtures"))?;
    std::fs::write(
        tools.join("sha256sum"),
        r#"#!/bin/sh
set -eu
case "$MCP_INSTALLER_HASH_FAILURE" in
    source) case "$1" in */release/mcp-cli) exit 37 ;; esac ;;
    staged) case "$1" in */.mcp-cli.install.*) exit 37 ;; esac ;;
    malformed) printf '%s\n' invalid; exit 0 ;;
    empty) exit 0 ;;
esac
case "$MCP_INSTALLER_REAL_HASH" in
    */shasum) exec "$MCP_INSTALLER_REAL_HASH" -a 256 "$@" ;;
    *) exec "$MCP_INSTALLER_REAL_HASH" "$@" ;;
esac
"#,
    )?;
    for tool in ["rustc", "cargo", "sha256sum"] {
        std::fs::set_permissions(tools.join(tool), std::fs::Permissions::from_mode(0o755))?;
    }
    let mut paths = vec![tools];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let path = std::env::join_paths(paths).map_err(io::Error::other)?;
    let original_source = std::fs::read(&source)?;
    for (build_failure, hash_failure) in [
        ("1", "none"),
        ("0", "source"),
        ("0", "staged"),
        ("0", "malformed"),
        ("0", "empty"),
        ("0", "none"),
    ] {
        std::fs::write(&destination, b"previous executable, not the new build")?;
        std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o711))?;
        let output = Command::new("sh")
            .arg(fixture.0.join("install.sh"))
            .arg("--bin-dir")
            .arg(
                destination
                    .parent()
                    .ok_or_else(|| io::Error::other("bin directory"))?,
            )
            .env("MCP_INSTALLER_FIXTURE", &fixture.0)
            .env("MCP_INSTALLER_BUILD_FAILURE", build_failure)
            .env("MCP_INSTALLER_HASH_FAILURE", hash_failure)
            .env("MCP_INSTALLER_REAL_HASH", &real_hash)
            .env("PATH", &path)
            .current_dir(&other_directory)
            .output()
            .await?;
        let succeeds = build_failure == "0" && hash_failure == "none";
        assert_eq!(
            output.status.success(),
            succeeds,
            "build={build_failure}, hash={hash_failure}, installer output: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let expected = if succeeds {
            original_source.as_slice()
        } else {
            b"previous executable, not the new build"
        };
        assert_eq!(std::fs::read(&destination)?, expected);
        assert_eq!(std::fs::read(&source)?, original_source);
        assert_eq!(
            std::fs::metadata(&destination)?.permissions().mode() & 0o777,
            if succeeds { 0o751 } else { 0o711 }
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
    assert!(String::from_utf8_lossy(&installed.stdout).contains(env!("MCP_CLI_BUILD_VERSION")));
    Ok(())
}
