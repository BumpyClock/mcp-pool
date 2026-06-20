use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// When set, diagnostic lines are also echoed to stderr. The foreground `serve`
/// daemon enables this so `mcp-pool serve --debug` shows logs in the terminal.
/// The proxy path never enables it: there, stdout is the raw MCP byte stream and
/// stderr is reserved for user-facing errors.
static STDERR: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// Mirror enabled diagnostics to stderr (in addition to the log file).
pub fn set_stderr_mirror(enabled: bool) {
    STDERR.store(enabled, Ordering::SeqCst);
}

/// Enable logging automatically when MCP_POOL_DEBUG is set to a truthy value.
pub fn init_from_env() {
    if let Ok(value) = std::env::var("MCP_POOL_DEBUG") {
        let truthy = matches!(value.as_str(), "1" | "true" | "TRUE" | "yes");
        set_enabled(truthy);
    }
}

pub fn log_dir() -> Option<PathBuf> {
    config::state_dir().ok().map(|dir| dir.join("logs"))
}

/// Append a diagnostic line to the log file when enabled, and mirror it to stderr
/// when the stderr mirror is on (foreground `serve --debug`). Never writes to
/// stdout: on the proxy path stdout carries the MCP byte stream.
pub fn log(message: impl AsRef<str>) {
    if !is_enabled() {
        return;
    }
    let message = message.as_ref();
    if STDERR.load(Ordering::SeqCst) {
        eprintln!("{message}");
    }
    let Some(dir) = log_dir() else {
        return;
    };
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("mcp-pool.log"))
    {
        let _ = writeln!(file, "{}", message);
    }
}
