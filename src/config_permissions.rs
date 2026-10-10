use std::path::Path;

use anyhow::{Context, Result, bail};

pub(super) struct Snapshot {
    permissions: std::fs::Permissions,
    #[cfg(windows)]
    security: WindowsSecurity,
}

impl Snapshot {
    pub(super) fn read(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path).context("Reading original config permissions")?;
        let permissions = file.metadata()?.permissions();
        if permissions.readonly() {
            bail!("Config is read-only; no changes made");
        }
        Ok(Self {
            permissions,
            #[cfg(windows)]
            security: WindowsSecurity::read(&file)?,
        })
    }

    pub(super) fn apply(&self, file: &std::fs::File) -> Result<()> {
        #[cfg(windows)]
        self.security.apply(file)?;
        file.set_permissions(self.permissions.clone())
            .context("Preserving config permissions")
    }
}

#[cfg(windows)]
struct WindowsSecurity {
    descriptor: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR,
    dacl: *mut windows_sys::Win32::Security::ACL,
    protected: bool,
}

#[cfg(windows)]
impl WindowsSecurity {
    fn read(file: &std::fs::File) -> Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorControl, SE_DACL_PROTECTED,
        };
        let mut descriptor = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(std::io::Error::from_raw_os_error(status as i32))
                .context("Reading config access control");
        }
        let mut snapshot = Self {
            descriptor,
            dacl,
            protected: false,
        };
        let mut control = 0;
        let mut revision = 0;
        if unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) } == 0 {
            return Err(std::io::Error::last_os_error()).context("Reading config ACL protection");
        }
        snapshot.protected = control & SE_DACL_PROTECTED != 0;
        Ok(snapshot)
    }

    fn apply(&self, file: &std::fs::File) -> Result<()> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Security::Authorization::{SE_FILE_OBJECT, SetSecurityInfo};
        use windows_sys::Win32::Security::{
            DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
            UNPROTECTED_DACL_SECURITY_INFORMATION,
        };
        let information = DACL_SECURITY_INFORMATION
            | if self.protected {
                PROTECTED_DACL_SECURITY_INFORMATION
            } else {
                UNPROTECTED_DACL_SECURITY_INFORMATION
            };
        let status = unsafe {
            SetSecurityInfo(
                file.as_raw_handle().cast(),
                SE_FILE_OBJECT,
                information,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                self.dacl,
                std::ptr::null_mut(),
            )
        };
        if status != 0 {
            return Err(std::io::Error::from_raw_os_error(status as i32))
                .context("Preserving config access control");
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for WindowsSecurity {
    fn drop(&mut self) {
        if !self.descriptor.is_null()
            && !unsafe { windows_sys::Win32::Foundation::LocalFree(self.descriptor.cast()) }
                .is_null()
        {
            eprintln!("[mcp-pool] Could not free config security descriptor");
        }
    }
}
