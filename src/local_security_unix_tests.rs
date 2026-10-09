use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn create() -> io::Result<Self> {
        let number = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::current_dir()?.join("target").join(format!(
            "mcp-pool-local-security-{}-{number}",
            std::process::id()
        ));
        fs::create_dir_all(path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "isolated test path has no parent",
            )
        })?)?;
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }

    fn socket(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.0) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("could not remove isolated socket test directory: {error}");
            }
        }
    }
}

#[tokio::test]
async fn creates_private_same_user_socket_and_directory() -> io::Result<()> {
    let directory = TestDirectory::create()?;
    let path = directory.socket("private.sock");
    let listener = bind_unix_listener(&path)?;
    let socket_metadata = fs::symlink_metadata(&path)?;
    assert!(socket_metadata.file_type().is_socket());
    assert_eq!(socket_metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(socket_metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        fs::metadata(&directory.0)?.permissions().mode() & 0o777,
        0o700
    );

    let endpoint = validate_unix_socket_path(&path)?;
    let client = tokio::net::UnixStream::connect(endpoint).await?;
    let (server, _) = listener.accept().await?;
    verify_unix_peer(&server)?;
    verify_unix_peer(&client)?;
    drop(client);
    drop(server);
    drop(listener);
    fs::remove_file(path)?;
    Ok(())
}

#[tokio::test]
async fn refuses_to_change_shared_or_unsafe_parent_directories() -> io::Result<()> {
    let directory = TestDirectory::create()?;
    fs::set_permissions(&directory.0, fs::Permissions::from_mode(0o777))?;
    let result = bind_unix_listener(&directory.socket("unsafe.sock"));
    assert_eq!(
        result.err().map(|error| error.kind()),
        Some(io::ErrorKind::PermissionDenied)
    );
    assert_eq!(
        fs::metadata(&directory.0)?.permissions().mode() & 0o777,
        0o777
    );
    Ok(())
}

#[tokio::test]
async fn hardens_only_the_application_socket_directories() -> io::Result<()> {
    let directory = TestDirectory::create()?;
    let state = directory.0.join("state");
    let application = state.join("mcp-pool");
    let run = application.join("run");
    fs::create_dir_all(&run)?;
    fs::set_permissions(&application, fs::Permissions::from_mode(0o755))?;
    fs::set_permissions(&run, fs::Permissions::from_mode(0o755))?;

    let path = run.join("pool.sock");
    let listener = bind_unix_listener(&path)?;
    assert_eq!(
        fs::metadata(&application)?.permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(fs::metadata(&run)?.permissions().mode() & 0o777, 0o700);
    assert_eq!(fs::metadata(&state)?.permissions().mode() & 0o777, 0o755);
    drop(listener);
    fs::remove_file(path)?;
    Ok(())
}

#[tokio::test]
async fn refuses_symlink_and_non_socket_endpoints_without_removing_them() -> io::Result<()> {
    let directory = TestDirectory::create()?;
    let target = directory.socket("target.sock");
    let listener = bind_unix_listener(&target)?;
    drop(listener);
    let link = directory.socket("link.sock");
    std::os::unix::fs::symlink(&target, &link)?;
    let error = validate_unix_socket_path(&link)
        .err()
        .ok_or_else(|| io::Error::other("symlink endpoint unexpectedly connected"))?;
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(fs::symlink_metadata(&link)?.file_type().is_symlink());

    let regular = directory.socket("regular.sock");
    fs::write(&regular, b"not a socket")?;
    let error = bind_unix_listener(&regular)
        .err()
        .ok_or_else(|| io::Error::other("regular file endpoint unexpectedly bound"))?;
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(fs::read(&regular)?, b"not a socket");

    let state = directory.0.join("state");
    let real_application = state.join("real-mcp-pool");
    let linked_application = state.join("mcp-pool");
    fs::create_dir_all(&real_application)?;
    std::os::unix::fs::symlink(&real_application, &linked_application)?;
    let linked_endpoint = linked_application.join("linked.sock");
    let error = prepare_unix_socket_path(&linked_endpoint)
        .err()
        .ok_or_else(|| io::Error::other("symlink socket parent unexpectedly prepared"))?;
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);

    let real_parent = directory.socket("real-parent");
    let linked_parent = directory.socket("linked-parent");
    fs::create_dir(&real_parent)?;
    std::os::unix::fs::symlink(&real_parent, &linked_parent)?;
    let error = prepare_unix_socket_path(&linked_parent.join("unsafe.sock"))
        .err()
        .ok_or_else(|| io::Error::other("private symlink parent unexpectedly prepared"))?;
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    Ok(())
}

#[tokio::test]
async fn removes_only_a_same_user_stale_socket() -> io::Result<()> {
    let directory = TestDirectory::create()?;
    let path = directory.socket("stale.sock");
    let listener = bind_unix_listener(&path)?;
    drop(listener);
    let replacement = bind_unix_listener(&path)?;
    assert!(fs::symlink_metadata(&path)?.file_type().is_socket());
    drop(replacement);
    fs::remove_file(path)?;
    Ok(())
}

#[tokio::test]
async fn rejects_foreign_owned_socket_without_removing_it_when_privileged() -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        eprintln!("foreign-owner socket test requires root");
        return Ok(());
    }

    let directory = TestDirectory::create()?;
    let path = directory.socket("foreign.sock");
    let listener = bind_unix_listener(&path)?;
    drop(listener);
    std::os::unix::fs::chown(&path, Some(65_534), Some(65_534))?;

    let error = bind_unix_listener(&path)
        .err()
        .ok_or_else(|| io::Error::other("foreign-owned endpoint unexpectedly rebound"))?;
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(fs::symlink_metadata(&path)?.uid(), 65_534);
    Ok(())
}
