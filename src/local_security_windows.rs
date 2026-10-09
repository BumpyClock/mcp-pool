use std::io;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::ptr;

use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer, ServerOptions};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE, LocalFree,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
};
use windows_sys::Win32::Security::{
    EqualSid, GetTokenInformation, IsValidSecurityDescriptor, SECURITY_ATTRIBUTES, TOKEN_QUERY,
    TOKEN_USER, TokenUser,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetNamedPipeServerProcessId(pipe: HANDLE, server_process_id: *mut u32) -> i32;
}

const MAX_TOKEN_INFORMATION_BYTES: u32 = 64 * 1024;

pub(crate) fn create_named_pipe(name: &str, first_instance: bool) -> io::Result<NamedPipeServer> {
    validate_pipe_name(name)?;
    let mut security = PipeSecurity::new()?;
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(first_instance)
        .reject_remote_clients(true);
    let result =
        unsafe { options.create_with_security_attributes_raw(name, security.attributes_pointer()) };
    let release_result = security.release();
    match (result, release_result) {
        (Ok(server), Ok(())) => Ok(server),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(io::Error::new(
            error.kind(),
            format!("{error}; security descriptor cleanup failed: {cleanup_error}"),
        )),
    }
}

pub(crate) fn connect_named_pipe(path: &Path) -> io::Result<NamedPipeClient> {
    let name = validate_pipe_name(path.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "named pipe path is not valid Unicode",
        )
    })?)?;
    let client = tokio::net::windows::named_pipe::ClientOptions::new().open(&name)?;
    verify_named_pipe_server(&client)?;
    Ok(client)
}

fn validate_pipe_name(name: &str) -> io::Result<String> {
    let prefix = r"\\.\pipe\";
    let Some(remainder) = name
        .get(..prefix.len())
        .filter(|candidate| candidate.eq_ignore_ascii_case(prefix))
        .map(|_| &name[prefix.len()..])
    else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "named pipe path must use the local pipe namespace",
        ));
    };
    if remainder.is_empty() || remainder.contains(['\\', '/']) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "named pipe path must contain one local pipe name",
        ));
    }
    Ok(name.to_owned())
}

fn verify_named_pipe_server(client: &NamedPipeClient) -> io::Result<()> {
    let mut server_process_id = 0;
    if unsafe {
        GetNamedPipeServerProcessId(client.as_raw_handle() as HANDLE, &mut server_process_id)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if server_process_id == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "named pipe server process identifier is invalid",
        ));
    }

    let mut process = OwnedHandle::empty();
    let mut server_token = OwnedHandle::empty();
    let mut current_token = OwnedHandle::empty();
    let verification = (|| {
        let process_handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, server_process_id) };
        if process_handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        process = OwnedHandle::new(process_handle);

        server_token = open_process_token(process.get())?;
        let server_user = token_user_information(server_token.get())?;

        current_token = open_process_token(unsafe { GetCurrentProcess() })?;
        let current_user = token_user_information(current_token.get())?;

        let server_sid = token_user_sid(&server_user)?;
        let current_sid = token_user_sid(&current_user)?;
        if unsafe { EqualSid(server_sid, current_sid) } == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "named pipe server belongs to another user",
            ));
        }
        Ok(())
    })();

    let current_close = current_token.close();
    let server_close = server_token.close();
    let process_close = process.close();
    finish_with_cleanup(
        verification,
        combine_cleanup_results([current_close, server_close, process_close]),
    )
}

fn open_process_token(process: HANDLE) -> io::Result<OwnedHandle> {
    let mut token = ptr::null_mut();
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if token.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned an empty process token",
        ));
    }
    Ok(OwnedHandle::new(token))
}

fn token_user_information(token: HANDLE) -> io::Result<Vec<usize>> {
    let mut required = 0;
    if unsafe { GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut required) } != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows returned an unexpected empty token-user result",
        ));
    }
    let error_code = unsafe { GetLastError() };
    if error_code != ERROR_INSUFFICIENT_BUFFER || required == 0 {
        return Err(io::Error::from_raw_os_error(error_code as i32));
    }
    if required > MAX_TOKEN_INFORMATION_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows token-user information exceeds its size limit",
        ));
    }

    let word_size = std::mem::size_of::<usize>();
    let word_count = (required as usize)
        .checked_add(word_size - 1)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "token size overflow"))?
        / word_size;
    let mut buffer = vec![0usize; word_count];
    if unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    token_user_sid(&buffer)?;
    Ok(buffer)
}

fn token_user_sid(buffer: &[usize]) -> io::Result<*mut core::ffi::c_void> {
    if std::mem::size_of_val(buffer) < std::mem::size_of::<TOKEN_USER>() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows token-user information is truncated",
        ));
    }
    let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    if user.User.Sid.is_null() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows token-user SID is missing",
        ));
    }
    Ok(user.User.Sid)
}

fn current_user_sid_string() -> io::Result<String> {
    let mut token = open_process_token(unsafe { GetCurrentProcess() })?;
    let conversion = (|| {
        let user = token_user_information(token.get())?;
        let sid = token_user_sid(&user)?;
        let mut string_sid = ptr::null_mut();
        if unsafe { ConvertSidToStringSidW(sid, &mut string_sid) } == 0 {
            return Err(io::Error::last_os_error());
        }
        if string_sid.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows returned an empty SID string",
            ));
        }
        let allocation = LocalAllocation::new(string_sid.cast());
        let value = allocation.utf16_string()?;
        finish_with_cleanup(Ok(value), allocation.release())
    })();
    finish_with_cleanup(conversion, token.close())
}

fn owner_only_pipe_sddl(user_sid: &str) -> String {
    format!("O:{user_sid}D:P(A;;GA;;;{user_sid})")
}

struct PipeSecurity {
    descriptor: LocalAllocation,
    attributes: SECURITY_ATTRIBUTES,
}

impl PipeSecurity {
    fn new() -> io::Result<Self> {
        let user_sid = current_user_sid_string()?;
        let sddl = owner_only_pipe_sddl(&user_sid);
        let security_text: Vec<u16> = sddl.encode_utf16().chain(Some(0)).collect();
        let mut descriptor = ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                security_text.as_ptr(),
                1,
                &mut descriptor,
                ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if descriptor.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Windows returned an empty pipe security descriptor",
            ));
        }

        let descriptor = LocalAllocation::new(descriptor);
        if unsafe { IsValidSecurityDescriptor(descriptor.get()) } == 0 {
            return finish_with_cleanup(
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Windows rejected the pipe security descriptor",
                )),
                descriptor.release(),
            );
        }
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.get(),
            bInheritHandle: 0,
        };
        Ok(Self {
            descriptor,
            attributes,
        })
    }

    fn attributes_pointer(&mut self) -> *mut core::ffi::c_void {
        (&mut self.attributes as *mut SECURITY_ATTRIBUTES).cast()
    }

    fn release(self) -> io::Result<()> {
        self.descriptor.release()
    }
}

struct LocalAllocation(*mut core::ffi::c_void);

impl LocalAllocation {
    fn new(pointer: *mut core::ffi::c_void) -> Self {
        Self(pointer)
    }

    fn get(&self) -> *mut core::ffi::c_void {
        self.0
    }

    fn utf16_string(&self) -> io::Result<String> {
        let pointer = self.0.cast::<u16>();
        let mut length = 0usize;
        while length < 256 {
            if unsafe { *pointer.add(length) } == 0 {
                let value = unsafe { std::slice::from_raw_parts(pointer, length) };
                return String::from_utf16(value)
                    .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
            }
            length += 1;
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Windows SID string exceeds its size limit",
        ))
    }

    fn release(mut self) -> io::Result<()> {
        if unsafe { LocalFree(self.0) }.is_null() {
            self.0 = ptr::null_mut();
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            if !unsafe { LocalFree(self.0) }.is_null() {
                eprintln!("LocalFree failed while releasing local IPC security memory");
            }
            self.0 = ptr::null_mut();
        }
    }
}

struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn empty() -> Self {
        Self(ptr::null_mut())
    }

    fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    fn get(&self) -> HANDLE {
        self.0
    }

    fn close(&mut self) -> io::Result<()> {
        let handle = self.0;
        if handle.is_null() {
            return Ok(());
        }
        if unsafe { CloseHandle(handle) } == 0 {
            Err(io::Error::last_os_error())
        } else {
            self.0 = ptr::null_mut();
            Ok(())
        }
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            if unsafe { CloseHandle(self.0) } == 0 {
                eprintln!("CloseHandle failed while releasing local IPC identity handle");
            }
            self.0 = ptr::null_mut();
        }
    }
}

fn combine_cleanup_results<const COUNT: usize>(results: [io::Result<()>; COUNT]) -> io::Result<()> {
    for result in results {
        result?;
    }
    Ok(())
}

fn finish_with_cleanup<T>(operation: io::Result<T>, cleanup: io::Result<()>) -> io::Result<T> {
    match (operation, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup_error)) => Err(io::Error::new(
            error.kind(),
            format!("{error}; local IPC cleanup failed: {cleanup_error}"),
        )),
    }
}

#[cfg(test)]
#[path = "local_security_windows_tests.rs"]
mod tests;
