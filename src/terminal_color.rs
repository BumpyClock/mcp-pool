pub(crate) fn stdout_supports_ansi() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;

        match enable_ansi(std::io::stdout().as_raw_handle()) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("mcp-pool: colored output unavailable; using plain output: {error}");
                false
            }
        }
    }
    #[cfg(not(windows))]
    {
        true
    }
}

#[cfg(windows)]
fn enable_ansi(handle: windows_sys::Win32::Foundation::HANDLE) -> std::io::Result<()> {
    use windows_sys::Win32::System::Console::{
        ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, SetConsoleMode,
    };

    let mut mode = 0;
    if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let enabled = mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    if enabled != mode && unsafe { SetConsoleMode(handle, enabled) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(test, windows))]
#[path = "terminal_color_tests.rs"]
mod tests;
