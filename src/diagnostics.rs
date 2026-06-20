use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::config;

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::SeqCst);
}

pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
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

/// Append a diagnostic line to the log file when enabled.
/// Never writes to stdout/stderr (stdout carries MCP data on the proxy path).
pub fn log(message: impl AsRef<str>) {
    if !is_enabled() {
        return;
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
        let _ = writeln!(file, "{}", message.as_ref());
    }
}
