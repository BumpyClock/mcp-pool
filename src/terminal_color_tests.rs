use std::ffi::OsString;
use std::io::{self, IsTerminal};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use windows_sys::Win32::Foundation::{
    GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle,
    STD_OUTPUT_HANDLE, SetConsoleMode, SetStdHandle,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_CONSOLE, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, GetExitCodeProcess,
    PROCESS_INFORMATION, STARTF_USESHOWWINDOW, STARTUPINFOW, TerminateProcess, WaitForSingleObject,
};

const FIXTURE: &str = "MCP_POOL_COLOR_FIXTURE";

fn wide(value: &std::ffi::OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn run_fixture(case: &str) -> io::Result<()> {
    let executable = std::env::current_exe()?;
    let application = wide(executable.as_os_str());
    let mut command = wide(
        OsString::from(format!(
            "\"{}\" --exact terminal_color::tests::isolated_console_fixture --test-threads=1 --nocapture",
            executable.display()
        ))
        .as_os_str(),
    );
    let mut variables = std::env::vars_os()
        .filter(|(name, _)| {
            !["NO_COLOR", "FORCE_COLOR", "TERM", FIXTURE]
                .iter()
                .any(|removed| name.eq_ignore_ascii_case(removed))
        })
        .collect::<Vec<_>>();
    variables.push((FIXTURE.into(), case.into()));
    match case {
        "no-color-env" => variables.push(("NO_COLOR".into(), "".into())),
        "force-zero" => variables.push(("FORCE_COLOR".into(), "0".into())),
        "dumb" => variables.push(("TERM".into(), "dumb".into())),
        _ => {}
    }
    variables.sort_by_key(|(name, _)| name.to_string_lossy().to_ascii_uppercase());
    let mut environment = Vec::new();
    for (name, value) in variables {
        let mut entry = name;
        entry.push("=");
        entry.push(value);
        environment.extend(wide(&entry));
    }
    environment.push(0);
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESHOWWINDOW,
        wShowWindow: 0,
        ..Default::default()
    };
    let mut information = PROCESS_INFORMATION::default();
    if unsafe {
        CreateProcessW(
            application.as_ptr(),
            command.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NEW_CONSOLE | CREATE_UNICODE_ENVIRONMENT,
            environment.as_ptr().cast(),
            std::ptr::null(),
            &startup,
            &mut information,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    let process = unsafe { OwnedHandle::from_raw_handle(information.hProcess) };
    let _thread = unsafe { OwnedHandle::from_raw_handle(information.hThread) };
    match unsafe { WaitForSingleObject(process.as_raw_handle(), 30_000) } {
        WAIT_OBJECT_0 => {}
        WAIT_TIMEOUT => {
            if unsafe { TerminateProcess(process.as_raw_handle(), 1) } == 0 {
                return Err(io::Error::last_os_error());
            }
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("isolated color fixture timed out: {case}"),
            ));
        }
        _ => return Err(io::Error::last_os_error()),
    }
    let mut exit_code = 0;
    if unsafe { GetExitCodeProcess(process.as_raw_handle(), &mut exit_code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    assert_eq!(exit_code, 0, "isolated color fixture: {case}");
    Ok(())
}

#[test]
fn isolated_console_enables_ansi_and_preserves_other_mode_bits() -> io::Result<()> {
    for case in ["enable", "already-enabled"] {
        run_fixture(case)?;
    }
    Ok(())
}

#[test]
fn isolated_console_color_failure_falls_back_to_plain_text() -> io::Result<()> {
    run_fixture("read-only")
}

#[test]
fn isolated_console_opt_outs_and_redirection_leave_mode_unchanged() -> io::Result<()> {
    for case in [
        "no-color",
        "no-color-env",
        "force-zero",
        "dumb",
        "redirected",
    ] {
        run_fixture(case)?;
    }
    Ok(())
}

#[test]
fn invalid_console_handle_reports_error() {
    assert!(super::enable_ansi(INVALID_HANDLE_VALUE).is_err());
}

struct RestoreStdout(HANDLE);

impl Drop for RestoreStdout {
    fn drop(&mut self) {
        if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, self.0) } == 0 {
            eprintln!(
                "could not restore isolated fixture stdout: {}",
                io::Error::last_os_error()
            );
        }
    }
}

#[test]
fn isolated_console_fixture() -> io::Result<()> {
    let Some(case) = std::env::var_os(FIXTURE) else {
        return Ok(());
    };
    let case = case
        .to_str()
        .ok_or_else(|| io::Error::other("invalid color fixture case"))?;
    let original = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
    let _restore = RestoreStdout(original);
    assert!(io::stdout().is_terminal());
    let mut mode = 0;
    if unsafe { GetConsoleMode(original, &mut mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mode = if case == "already-enabled" {
        mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING
    } else {
        mode & !ENABLE_VIRTUAL_TERMINAL_PROCESSING
    };
    if unsafe { SetConsoleMode(original, mode) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let replacement = match case {
        "read-only" | "already-enabled" => {
            let console = wide(std::ffi::OsStr::new("CONOUT$"));
            let handle = unsafe {
                CreateFileW(
                    console.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            Some(unsafe { OwnedHandle::from_raw_handle(handle) })
        }
        "redirected" => Some(std::fs::OpenOptions::new().write(true).open("NUL")?.into()),
        _ => None,
    };
    if let Some(handle) = &replacement
        && unsafe { SetStdHandle(STD_OUTPUT_HANDLE, handle.as_raw_handle()) } == 0
    {
        return Err(io::Error::last_os_error());
    }
    assert_eq!(io::stdout().is_terminal(), case != "redirected");
    let text = crate::tool_documentation::Style::terminal(case == "no-color").heading("fixture");
    let mut after = 0;
    if unsafe { GetConsoleMode(original, &mut after) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if matches!(case, "enable" | "already-enabled") {
        assert_eq!(text, "\x1b[1mfixture\x1b[0m");
        assert_eq!(
            after,
            mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING
        );
        assert!(super::stdout_supports_ansi());
    } else {
        assert_eq!(text, "fixture");
        assert_eq!(after, mode);
    }
    if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, original) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
