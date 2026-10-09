use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(super) fn digest(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
        .chars()
        .take(16)
        .collect()
}

pub(super) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

pub(super) fn timestamp() -> String {
    let seconds = now();
    let days = (seconds / 86_400) as i64 + 719_468;
    let era = days / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z",
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

pub(super) fn read_json(path: &Path) -> Result<Option<Value>> {
    read_text(path)?
        .map(|text| {
            serde_json::from_str(&text).map_err(|_| {
                anyhow::anyhow!(
                    "credential JSON is malformed; repair the selected store before authorization; existing contents were not modified"
                )
            })
        })
        .transpose()
}

pub(super) fn read_text(path: &Path) -> Result<Option<String>> {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("could not read credential store"),
    };
    if file.metadata()?.len() > 8 * 1024 * 1024 {
        bail!("credential store exceeds size limit");
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .context("could not read credential store")?;
    Ok(Some(text))
}

pub(super) fn canonical(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return fs::canonicalize(path).context("could not resolve credential path");
    }
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("credential path has no parent"))?;
    let parent = if parent.exists() {
        fs::canonicalize(parent)?
    } else {
        std::path::absolute(parent)?
    };
    Ok(parent.join(
        path.file_name()
            .ok_or_else(|| anyhow::anyhow!("credential path has no filename"))?,
    ))
}

pub(super) fn portable_path(path: &Path) -> String {
    let value = path.to_string_lossy();
    value.strip_prefix(r"\\?\").unwrap_or(&value).to_owned()
}

pub(super) fn write_json(path: &Path, value: &Value) -> Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value)?)
}

pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let target = canonical(path)?;
    let parent = target
        .parent()
        .ok_or_else(|| anyhow::anyhow!("credential store has no parent"))?;
    create_directory(parent)?;
    let suffix = oauth2::CsrfToken::new_random().secret().to_owned();
    let staging = parent.join(format!(".mcp-pool-{}-{suffix}.pending", std::process::id()));
    let result = (|| {
        let mut file = private_file(&staging)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        replace(&staging, &target)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() && staging.exists() {
        fs::remove_file(&staging).context("could not remove incomplete credential write")?;
    }
    result
}

fn create_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
        if path.is_dir() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            create_directory(parent)?;
        }
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        with_security(|attributes| {
            if unsafe { CreateDirectoryW(wide.as_ptr(), attributes) } == 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
            Ok(())
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn private_file(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(windows)]
fn private_file(path: &Path) -> std::io::Result<File> {
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CREATE_NEW, CreateFileW, FILE_ATTRIBUTE_NORMAL};
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    with_security(|attributes| {
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_WRITE,
                0,
                attributes,
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                std::ptr::null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { File::from_raw_handle(handle) })
    })
}

#[cfg(windows)]
fn with_security<T>(
    operation: impl FnOnce(&windows_sys::Win32::Security::SECURITY_ATTRIBUTES) -> std::io::Result<T>,
) -> std::io::Result<T> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    let security: Vec<u16> = "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;OW)\0"
        .encode_utf16()
        .collect();
    let mut descriptor = std::ptr::null_mut();
    // The protected DACL is applied at creation, before any secret is written.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            security.as_ptr(),
            1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: 0,
    };
    let result = operation(&attributes);
    unsafe {
        LocalFree(descriptor);
    }
    result
}

#[cfg(unix)]
fn replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    fs::rename(source, destination)
}

#[cfg(windows)]
fn replace(source: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

pub(super) struct Lock {
    path: PathBuf,
    contents: String,
}

pub(super) struct AsyncLocks(Option<Vec<Lock>>);

impl AsyncLocks {
    pub(super) fn new(locks: Vec<Lock>) -> Self {
        Self(Some(locks))
    }
    pub(super) async fn release(mut self) -> Result<()> {
        let locks = self.0.take();
        tokio::task::spawn_blocking(move || drop(locks)).await?;
        Ok(())
    }
}

impl Drop for AsyncLocks {
    fn drop(&mut self) {
        if let Some(locks) = self.0.take() {
            // Cancellation/error paths must not perform Windows retry sleeps on the reactor.
            tokio::task::spawn_blocking(move || drop(locks));
        }
    }
}

impl Lock {
    pub(super) fn acquire(target: &Path) -> Result<Self> {
        let target = canonical(target)?;
        let parent = target
            .parent()
            .ok_or_else(|| anyhow::anyhow!("lock has no parent"))?;
        create_directory(parent)?;
        let path = PathBuf::from(format!("{}.lock", target.to_string_lossy()));
        let contents = format!(
            "{}\n{}\n{}\n",
            std::process::id(),
            timestamp(),
            oauth2::CsrfToken::new_random().secret()
        );
        let started = Instant::now();
        loop {
            match private_file(&path) {
                Ok(mut file) => {
                    if let Err(error) = file.write_all(contents.as_bytes()) {
                        drop(file);
                        fs::remove_file(&path)
                            .context("could not release incomplete credential lock")?;
                        return Err(error.into());
                    }
                    return Ok(Self { path, contents });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if started.elapsed() > Duration::from_secs(30) {
                        bail!(
                            "credential transaction is busy; retry later (an abandoned mcporter lock may require manual cleanup)"
                        );
                    }
                    std::thread::sleep(Duration::from_millis(25));
                }
                Err(_) => bail!("could not acquire credential transaction lock"),
            }
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        for _attempt in 0..20 {
            match read_text(&self.path) {
                Ok(Some(contents)) if contents == self.contents => {}
                Ok(_) => return,
                Err(_) => {
                    eprintln!("could not verify credential lock ownership");
                    return;
                }
            }
            match fs::remove_file(&self.path) {
                Ok(()) => return,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
                Err(_) => std::thread::sleep(Duration::from_millis(25)),
            }
        }
        eprintln!("could not release credential lock; manual cleanup may be required");
    }
}
