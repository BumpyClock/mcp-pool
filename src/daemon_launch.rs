use anyhow::{Context, Result};
use std::ffi::OsString;

use super::options::Options;

pub(super) async fn start(options: &Options) -> Result<()> {
    if options.foreground {
        if let Some(path) = &options.log_file {
            super::logging::configure(path.clone(), options.servers.clone())?;
        }
        return crate::daemon::serve().await;
    }
    let mut arguments: Vec<OsString> = ["daemon", "start", "--foreground"]
        .into_iter()
        .map(OsString::from)
        .collect();
    if options.log {
        arguments.push("--log".into());
    }
    if let Some(path) = &options.log_file {
        arguments.push("--log-file".into());
        arguments.push(path.as_os_str().to_owned());
    }
    if !options.servers.is_empty() {
        arguments.push("--log-servers".into());
        arguments.push(
            options
                .servers
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(",")
                .into(),
        );
    }
    spawn_detached(&arguments)?;
    let expires = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if super::direct(&crate::control::ControlRequest::Status { name: None })
            .await?
            .is_some()
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= expires {
            anyhow::bail!("Daemon did not become ready within 10 seconds");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

#[cfg(unix)]
pub(crate) fn spawn_detached(arguments: &[OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .args(arguments)
        .process_group(0)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command
        .spawn()
        .context("Starting the existing mcp-pool broker")?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn spawn_detached(arguments: &[OsString]) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION,
        STARTUPINFOW,
    };
    let executable = std::env::current_exe()?;
    let application: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut command = quote_windows(executable.as_os_str());
    for argument in arguments {
        if argument.encode_wide().any(|character| character == 0) {
            anyhow::bail!("Daemon launch arguments must not contain NUL");
        }
        command.push(u16::from(b' '));
        command.extend(quote_windows(argument));
    }
    command.push(0);
    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..unsafe { std::mem::zeroed() }
    };
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // A daemon must not retain the caller's captured pipe handles after it exits.
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP,
            std::ptr::null(),
            std::ptr::null(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error()).context("Starting detached mcp-pool broker");
    }
    let mut error = None;
    for handle in [process.hThread, process.hProcess] {
        if unsafe { CloseHandle(handle) } == 0 {
            error.get_or_insert_with(std::io::Error::last_os_error);
        }
    }
    if let Some(error) = error {
        return Err(error).context("Closing daemon launch handles");
    }
    Ok(())
}

#[cfg(windows)]
fn quote_windows(argument: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    let mut quoted = vec![u16::from(b'"')];
    let mut backslashes = 0usize;
    for character in argument.encode_wide() {
        if character == u16::from(b'\\') {
            backslashes += 1;
            continue;
        }
        let count = if character == u16::from(b'"') {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        quoted.extend(std::iter::repeat_n(u16::from(b'\\'), count));
        quoted.push(character);
        backslashes = 0;
    }
    quoted.extend(std::iter::repeat_n(u16::from(b'\\'), backslashes * 2));
    quoted.push(u16::from(b'"'));
    quoted
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    #[test]
    fn detached_command_arguments_are_quoted_without_shell_interpretation() -> Result<()> {
        let quote = |value: &str| {
            String::from_utf16(&quote_windows(std::ffi::OsStr::new(value)))
                .map_err(anyhow::Error::from)
        };
        assert_eq!(quote("server with spaces")?, "\"server with spaces\"");
        assert_eq!(quote(r"C:\folder\")?, r#""C:\folder\\""#);
        assert_eq!(quote(r#"a"b"#)?, r#""a\"b""#);
        Ok(())
    }
}
