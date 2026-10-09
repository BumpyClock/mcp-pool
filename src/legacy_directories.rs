use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub(super) fn directories() -> Result<Vec<PathBuf>> {
    if let Some(directory) =
        std::env::var_os("MCPORTER_DAEMON_DIR").filter(|value| !value.is_empty())
    {
        let directory = PathBuf::from(directory);
        let directory = if directory.is_absolute() {
            directory
        } else {
            std::env::current_dir()?.join(directory)
        };
        return Ok(vec![directory.join("daemon")]);
    }
    let home =
        dirs::home_dir().context("Home directory unavailable for legacy daemon inventory")?;
    let mut directories = vec![home.join(".mcporter").join("daemon")];
    if let Some(state) = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
    {
        directories.push(state.join("mcporter").join("daemon"));
    }
    directories.sort();
    directories.dedup();
    Ok(directories)
}

pub(super) fn inspect(directory: &Path) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).context("Inspecting legacy daemon directory"),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("Legacy daemon directory must be a real owned directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() }
            || !matches!(metadata.mode() & 0o7777, 0o700 | 0o755)
        {
            bail!("Legacy daemon directory must be current-user-owned with mode 0700 or 0755");
        }
    }
    #[cfg(windows)]
    verify_windows_directory(directory)?;
    Ok(true)
}

pub(super) fn inspect_file(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() || metadata.len() > 1024 * 1024 {
        bail!("Legacy metadata must be a regular owned file no larger than 1 MiB");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != unsafe { libc::geteuid() } {
            bail!("Legacy metadata owner is unverified");
        }
    }
    Ok(())
}

pub(super) fn upgrade(directory: &Path) -> Result<()> {
    if !inspect(directory)? {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
        let previous = std::fs::symlink_metadata(directory)?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(directory)?;
        let opened = file.metadata()?;
        if previous.dev() != opened.dev() || previous.ino() != opened.ino() {
            bail!("Legacy directory identity changed during migration");
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o700))?;
        let current = std::fs::symlink_metadata(directory)?;
        if current.dev() != opened.dev()
            || current.ino() != opened.ino()
            || current.mode() & 0o7777 != 0o700
        {
            bail!("Legacy directory permission upgrade could not be verified");
        }
    }
    Ok(())
}

#[cfg(windows)]
#[allow(
    clippy::disallowed_methods,
    reason = "Directory inspection runs on a blocking worker"
)]
fn verify_windows_directory(directory: &Path) -> Result<()> {
    let script = r#"
$ErrorActionPreference='Stop'
$path=$env:MCP_POOL_MIGRATION_DIRECTORY
$owner=[Security.Principal.WindowsIdentity]::GetCurrent().User
$acl=Get-Acl -LiteralPath $path
if($acl.GetOwner([Security.Principal.SecurityIdentifier]).Value -ne $owner.Value){throw 'Foreign directory owner'}
$allowed=@($owner.Value,'S-1-5-18','S-1-5-32-544')
$write=[Security.AccessControl.FileSystemRights]::Write -bor [Security.AccessControl.FileSystemRights]::Delete -bor [Security.AccessControl.FileSystemRights]::ChangePermissions -bor [Security.AccessControl.FileSystemRights]::TakeOwnership
foreach($rule in $acl.Access){
  $sid=$rule.IdentityReference.Translate([Security.Principal.SecurityIdentifier]).Value
  if($rule.AccessControlType -eq 'Allow' -and ($rule.FileSystemRights -band $write) -ne 0 -and $allowed -notcontains $sid){throw 'Foreign writer'}
}
"#;
    let root = std::env::var_os("SystemRoot").context("Windows system directory unavailable")?;
    let executable = PathBuf::from(root)
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
    let output = std::process::Command::new(executable)
        .env("MCP_POOL_MIGRATION_DIRECTORY", directory)
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()?;
    if !output.status.success() {
        bail!(
            "Legacy directory ownership or writer ACL cannot be verified; repair owner-only permissions before migration"
        );
    }
    Ok(())
}
