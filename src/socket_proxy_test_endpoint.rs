use super::*;

static NEXT_ENDPOINT: AtomicU32 = AtomicU32::new(0);

pub(super) struct Endpoint {
    path: PathBuf,
    #[cfg(unix)]
    directory: PathBuf,
}

impl Endpoint {
    fn create() -> io::Result<Self> {
        let next_identifier = || {
            format!(
                "{}-{:x}",
                std::process::id(),
                NEXT_ENDPOINT.fetch_add(1, Ordering::Relaxed)
            )
        };
        #[cfg(windows)]
        {
            let identifier = next_identifier();
            Ok(Self {
                path: PathBuf::from(format!(r"\\.\pipe\mcp-cli-test-{identifier}")),
            })
        }
        #[cfg(unix)]
        loop {
            use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
            let identifier = next_identifier();
            let directory = PathBuf::from("/tmp").join(format!("mcp-test-{identifier}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {
                    let endpoint = Self {
                        path: directory.join("s"),
                        directory,
                    };
                    std::fs::set_permissions(
                        &endpoint.directory,
                        std::fs::Permissions::from_mode(0o700),
                    )?;
                    return Ok(endpoint);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(unix)]
impl Drop for Endpoint {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.directory)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("could not remove isolated socket fixture: {error}");
        }
    }
}

impl SocketProxy {
    pub(crate) fn fixture(name: String, specification: UpstreamSpec) -> io::Result<Arc<Self>> {
        let endpoint = Endpoint::create()?;
        let mut proxy = Self::new(name, endpoint.path.clone(), specification, None);
        proxy._test_endpoint = Some(endpoint);
        Ok(Arc::new(proxy))
    }
}

#[cfg(unix)]
#[test]
fn endpoints_are_private_distinct_and_owned_until_the_final_proxy_drop() -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let specification = UpstreamSpec::Http {
        url: "https://example.invalid/mcp".to_owned(),
        sse: false,
        headers: Default::default(),
        timeout_ms: None,
        auth: None,
    };
    let first = SocketProxy::fixture("first".to_owned(), specification.clone())?;
    let second = SocketProxy::fixture("second".to_owned(), specification)?;
    let first_path = first.socket_path();
    let directory = first_path
        .parent()
        .ok_or_else(|| io::Error::other("fixture directory missing"))?;
    assert_ne!(first_path, second.socket_path());
    assert_eq!(
        std::fs::metadata(directory)?.permissions().mode() & 0o777,
        0o700
    );
    assert!(first_path.as_os_str().as_encoded_bytes().len() < 104);
    let retained = first.clone();
    drop(first);
    assert!(directory.try_exists()?);
    drop(retained);
    assert!(!directory.try_exists()?);
    Ok(())
}
