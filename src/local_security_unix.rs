use std::fs::{self, Metadata};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};

static UMASK_LOCK: parking_lot::Mutex<()> = parking_lot::const_mutex(());

pub(crate) fn bind_unix_listener(path: &Path) -> io::Result<tokio::net::UnixListener> {
    let endpoint = prepare_unix_socket_path(path)?;
    match bind_with_private_mode(&endpoint) {
        Ok(listener) => {
            validate_socket_file(&endpoint)?;
            Ok(listener)
        }
        Err(bind_error) if bind_error.kind() == io::ErrorKind::AddrInUse => {
            let identity = inspect_socket_file(&endpoint)?;
            if probe_live_socket(&endpoint)? {
                return Err(bind_error);
            }

            remove_stale_socket(&endpoint, identity.as_ref())?;
            let listener = bind_with_private_mode(&endpoint)?;
            validate_socket_file(&endpoint)?;
            Ok(listener)
        }
        Err(error) => Err(error),
    }
}

pub(crate) fn validate_unix_socket_path(path: &Path) -> io::Result<PathBuf> {
    let endpoint = resolve_unix_socket_path(path)?;
    validate_socket_file(&endpoint)?;
    Ok(endpoint)
}

pub(crate) fn verify_unix_peer<T: std::os::fd::AsRawFd>(stream: &T) -> io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
        let mut credential_length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut credential_length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        if credential_length as usize != std::mem::size_of::<libc::ucred>() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "local peer credentials have an unexpected size",
            ));
        }
        let credentials = unsafe { credentials.assume_init() };
        if credentials.uid != unsafe { libc::geteuid() } {
            return Err(permission_denied(
                "local socket peer belongs to another user",
            ));
        }
        return Ok(());
    }

    #[cfg(any(
        target_vendor = "apple",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        let mut user_id = 0;
        let mut group_id = 0;
        if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut user_id, &mut group_id) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if user_id != unsafe { libc::geteuid() } {
            return Err(permission_denied(
                "local socket peer belongs to another user",
            ));
        }
        return Ok(());
    }

    #[allow(unreachable_code)]
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "this Unix platform cannot verify local socket peer credentials",
    ))
}

pub(crate) fn prepare_unix_socket_path(path: &Path) -> io::Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "local socket path has no parent",
        )
    })?;
    let file_name = socket_file_name(path)?;
    reject_app_directory_symlinks(parent)?;
    reject_unsafe_parent_symlink(parent)?;
    let parent = create_private_directories(parent)?;
    secure_socket_directory(&parent, true)?;
    Ok(parent.join(file_name))
}

fn resolve_unix_socket_path(path: &Path) -> io::Result<PathBuf> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "local socket path has no parent",
        )
    })?;
    let file_name = socket_file_name(path)?;
    reject_app_directory_symlinks(parent)?;
    reject_unsafe_parent_symlink(parent)?;
    let parent = fs::canonicalize(parent)?;
    secure_socket_directory(&parent, false)?;
    Ok(parent.join(file_name))
}

fn reject_app_directory_symlinks(path: &Path) -> io::Result<()> {
    for directory in app_owned_directories(path) {
        match fs::symlink_metadata(directory) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(permission_denied(
                    "application socket directory cannot be a symlink",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn reject_unsafe_parent_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let resolved = fs::canonicalize(path)?;
            let target = fs::metadata(resolved)?;
            let mode = target.permissions().mode();
            if !target.is_dir() || mode & 0o1000 == 0 || mode & 0o022 == 0 {
                return Err(permission_denied(
                    "local socket parent cannot be a symlink to a private directory",
                ));
            }
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn socket_file_name(path: &Path) -> io::Result<&std::ffi::OsStr> {
    if path
        .components()
        .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "local socket path cannot contain dot components",
        ));
    }
    path.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "local socket path has no filename",
        )
    })
}

fn create_private_directories(path: &Path) -> io::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let mut existing = absolute;
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&existing) {
            Ok(metadata) => {
                if !metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotADirectory,
                        "local socket parent is not a directory",
                    ));
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let component = existing.file_name().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "no existing socket parent")
                })?;
                missing.push(component.to_os_string());
                existing = existing
                    .parent()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::NotFound, "no existing socket parent")
                    })?
                    .to_path_buf();
            }
            Err(error) => return Err(error),
        }
    }

    let mut current = fs::canonicalize(existing)?;
    if !fs::metadata(&current)?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "local socket parent is not a directory",
        ));
    }
    for component in missing.into_iter().rev() {
        current.push(component);
        match fs::DirBuilder::new().mode(0o700).create(&current) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let metadata = fs::symlink_metadata(&current)?;
                if !metadata.file_type().is_dir() {
                    return Err(permission_denied(
                        "local socket parent contains an unsafe path component",
                    ));
                }
            }
            Err(error) => return Err(error),
        }
    }
    fs::canonicalize(current)
}

fn secure_socket_directory(path: &Path, repair_owned_directories: bool) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(permission_denied(
            "local socket parent is not a real directory",
        ));
    }

    let user_id = unsafe { libc::geteuid() };
    let app_directories = app_owned_directories(path);
    if !app_directories.is_empty() {
        if let Some(application_parent) = app_directories.first().and_then(|path| path.parent()) {
            verify_generic_directory(application_parent, user_id)?;
        }
        for directory in app_directories {
            let directory_metadata = fs::symlink_metadata(&directory)?;
            if !directory_metadata.file_type().is_dir() || directory_metadata.uid() != user_id {
                return Err(permission_denied(
                    "application socket directory is unsafe or belongs to another user",
                ));
            }
            if repair_owned_directories && directory_metadata.permissions().mode() & 0o7777 != 0o700
            {
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
            }
            let verified = fs::symlink_metadata(&directory)?;
            if !verified.file_type().is_dir()
                || verified.uid() != user_id
                || verified.permissions().mode() & 0o7777 != 0o700
            {
                return Err(permission_denied(
                    "application socket directory is not private",
                ));
            }
        }
        return Ok(());
    }

    verify_generic_directory(path, user_id)
}

fn verify_generic_directory(path: &Path, user_id: u32) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_dir() {
        return Err(permission_denied(
            "local socket parent is not a real directory",
        ));
    }
    let mode = metadata.permissions().mode();
    if (metadata.uid() == user_id && mode & 0o022 == 0) || (mode & 0o1000 != 0 && mode & 0o022 != 0)
    {
        return Ok(());
    }
    Err(permission_denied(
        "local socket parent is not private or sticky-shared",
    ))
}

fn app_owned_directories(path: &Path) -> Vec<PathBuf> {
    let mut directories = Vec::new();
    match path.file_name().and_then(std::ffi::OsStr::to_str) {
        Some("mcp-pool") => directories.push(path.to_path_buf()),
        Some("run")
            if path
                .parent()
                .and_then(Path::file_name)
                .and_then(std::ffi::OsStr::to_str)
                == Some("mcp-pool") =>
        {
            if let Some(application_directory) = path.parent() {
                directories.push(application_directory.to_path_buf());
            }
            directories.push(path.to_path_buf());
        }
        _ => {}
    }
    directories
}

fn bind_with_private_mode(path: &Path) -> io::Result<tokio::net::UnixListener> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "local socket path has no parent")
    })?;
    let metadata = fs::symlink_metadata(parent)?;
    if metadata.file_type().is_dir()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.permissions().mode() & 0o077 == 0
    {
        // The private parent prevents other users from reaching the socket before chmod.
        let listener = tokio::net::UnixListener::bind(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        return Ok(listener);
    }

    let _lock = UMASK_LOCK.lock();
    let umask = PrivateUmask::set();
    let result = tokio::net::UnixListener::bind(path);
    drop(umask);
    result
}

struct PrivateUmask(libc::mode_t);

impl PrivateUmask {
    fn set() -> Self {
        Self(unsafe { libc::umask(0o177) })
    }
}

impl Drop for PrivateUmask {
    fn drop(&mut self) {
        unsafe {
            libc::umask(self.0);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SocketIdentity {
    device: u64,
    inode: u64,
    owner: u32,
}

fn inspect_socket_file(path: &Path) -> io::Result<Option<SocketIdentity>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            validate_socket_metadata(&metadata)?;
            Ok(Some(SocketIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
                owner: metadata.uid(),
            }))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn validate_socket_file(path: &Path) -> io::Result<()> {
    if inspect_socket_file(path)?.is_some() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "local socket endpoint does not exist",
        ))
    }
}

fn validate_socket_metadata(metadata: &Metadata) -> io::Result<()> {
    if !metadata.file_type().is_socket() {
        return Err(permission_denied(
            "local socket path is not a socket or is a symlink",
        ));
    }
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(permission_denied(
            "local socket endpoint belongs to another user",
        ));
    }
    if metadata.permissions().mode() & 0o7777 != 0o600 {
        return Err(permission_denied(
            "local socket endpoint does not have private mode 0600",
        ));
    }
    Ok(())
}

fn probe_live_socket(path: &Path) -> io::Result<bool> {
    match UnixStream::connect(path) {
        Ok(stream) => {
            verify_unix_peer(&stream)?;
            Ok(true)
        }
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn remove_stale_socket(path: &Path, expected: Option<&SocketIdentity>) -> io::Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let Some(current) = inspect_socket_file(path)? else {
        return Ok(());
    };
    if &current != expected {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            "local socket endpoint changed during stale-socket probe",
        ));
    }
    fs::remove_file(path)
}

fn permission_denied(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

#[cfg(test)]
#[path = "local_security_unix_tests.rs"]
mod tests;
